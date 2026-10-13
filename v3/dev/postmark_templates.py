#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Keep BMAC's Postmark email templates in step with v3/notify/templates.

    v3/dev/postmark_templates.py check                 the manifest and files only; no network
    v3/dev/postmark_templates.py list                  every template on the server
    v3/dev/postmark_templates.py diff                  the manifest against the server
    v3/dev/postmark_templates.py validate              Postmark's check of each template
    v3/dev/postmark_templates.py sync [--apply]        create missing, update drafts
    v3/dev/postmark_templates.py retire ALIAS --apply  delete one old version
    v3/dev/postmark_templates.py send ALIAS --to ADDR --from ADDR --apply
                                                       one email rendered from the test model
                                                       (counts against the account's quota)
    v3/dev/postmark_templates.py release ALIAS         mark a template released (manifest only)

The server token comes from POSTMARK_SERVER_TOKEN and is never printed.
`send` takes its From address from --from or POSTMARK_SENDER, which must be
a confirmed Sender Signature of the account (even on a sandbox server).
Changing anything needs --apply and the server's name, from --server or
POSTMARK_SERVER_NAME. `lab/lab postmark ...` runs this with the lab's
sandbox server token. See MONITORING.md, "Email templates".
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

V3_ROOT = Path(__file__).resolve().parent.parent
TEMPLATES = V3_ROOT / "notify" / "templates"
API = "https://api.postmarkapp.com"
PREFIX = "bmac-"
LIMIT = 100
ALIAS = re.compile(r"^bmac-[a-z0-9]+(?:-[a-z0-9]+)*-v[1-9][0-9]*$")
TAG = re.compile(r"(\{\{\{?)\s*([#/^]?)\s*([^{}]*?)\s*(\}?\}\})")
SECRET_SHAPED = [
    re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", re.I),
    re.compile(r"tskey-"),
    re.compile(r"-----BEGIN"),
    re.compile(r"\b[A-Za-z0-9+/]{32,}={0,2}"),
    re.compile(r"(?i)password|secret|token"),
]


class TemplateError(Exception):
    pass


@dataclass
class Template:
    alias: str
    name: str
    type: str
    state: str
    fields: dict[str, Any]
    test_model: dict[str, Any]
    layout: str | None = None
    hash: str | None = None
    subject: str = ""
    text: str = ""
    html: str = ""
    problems: list[str] = field(default_factory=list)

    def body(self) -> dict[str, Any]:
        """The Templates API's create and edit body."""
        out: dict[str, Any] = {
            "Name": self.name,
            "Alias": self.alias,
            "HtmlBody": self.html,
            "TextBody": self.text,
            "TemplateType": self.type,
        }
        if self.type == "Standard":
            out["Subject"] = self.subject
            out["LayoutTemplate"] = self.layout
        return out


def strip_header(text: str) -> str:
    """The license header is for the repository, not for email: drop a
    leading block naming SPDX-License-Identifier, through its blank line."""
    head, sep, rest = text.partition("\n\n")
    if sep and "SPDX-License-Identifier" in head:
        return rest
    return text


def content_hash(template: Template) -> str:
    digest = hashlib.sha256()
    for part in (template.subject, template.text, template.html):
        digest.update(part.encode())
        digest.update(b"\0")
    return digest.hexdigest()


def load(root: Path = TEMPLATES) -> list[Template]:
    manifest = json.loads((root / "manifest.json").read_text(encoding="utf-8"))
    templates = []
    for entry in manifest["templates"]:
        template = Template(
            alias=entry["alias"],
            name=entry["name"],
            type=entry["type"],
            state=entry["state"],
            fields=entry["fields"],
            test_model=entry["test_model"],
            layout=entry.get("layout"),
            hash=entry.get("hash"),
        )
        directory = root / template.alias
        if template.type == "Standard":
            template.subject = strip_header((directory / "subject.txt").read_text(encoding="utf-8")).strip()
        template.text = strip_header((directory / "body.txt").read_text(encoding="utf-8"))
        template.html = strip_header((directory / "body.html").read_text(encoding="utf-8"))
        templates.append(template)
    return templates


def references(text: str) -> tuple[set[str], dict[str, set[str]], list[str]]:
    """Top-level names, names inside each `{{#each list}}`, and raw `{{{ }}}` tags."""
    top: set[str] = set()
    lists: dict[str, set[str]] = {}
    raw: list[str] = []
    stack: list[str] = []
    for opening, kind, name, closing in TAG.findall(text):
        if opening == "{{{" or closing == "}}}":
            raw.append(name)
            if name == "@content":
                continue
        if kind == "#":
            words = name.split()
            if len(words) == 2 and words[0] == "each":
                (lists[stack[-1]] if stack else top).add(words[1])
                stack.append(words[1])
                lists.setdefault(words[1], set())
                continue
            raise TemplateError(f"only {{{{#each}}}} sections are used, not {{{{#{name}}}}}")
        if kind == "^":
            raise TemplateError(f"inverted sections are not used: {{{{^{name}}}}}")
        if kind == "/":
            if not stack:
                raise TemplateError(f"{{{{/{name}}}}} closes nothing")
            stack.pop()
            continue
        if stack:
            lists[stack[-1]].add(name)
        else:
            top.add(name)
    if stack:
        raise TemplateError(f"{{{{#each {stack[-1]}}}}} is never closed")
    return top, lists, raw


def shape(fields: dict[str, Any]) -> dict[str, Any]:
    return {
        name: [shape(kind[0])] if isinstance(kind, list) else "scalar"
        for name, kind in fields.items()
    }


def model_shape(model: Any) -> Any:
    if isinstance(model, dict):
        return {name: model_shape(value) for name, value in model.items()}
    if isinstance(model, list):
        return [model_shape(model[0])] if model else []
    return "scalar"


def strings(model: Any) -> list[str]:
    if isinstance(model, dict):
        return [s for value in model.values() for s in strings(value)]
    if isinstance(model, list):
        return [s for value in model for s in strings(value)]
    return [model] if isinstance(model, str) else []


def problems(templates: list[Template]) -> list[str]:
    """Everything wrong with the manifest and files, with no network."""
    found: list[str] = []
    seen: set[str] = set()
    layouts = {t.alias: t for t in templates if t.type == "Layout"}
    for template in templates:
        alias = template.alias
        say = lambda message: found.append(f"{alias}: {message}")  # noqa: E731
        if alias in seen:
            say("listed twice")
        seen.add(alias)
        if not ALIAS.match(alias):
            say("aliases are bmac-<name>-v<N>")
        if template.type not in ("Standard", "Layout"):
            say(f"type {template.type!r} is neither Standard nor Layout")
        if template.state not in ("draft", "released"):
            say(f"state {template.state!r} is neither draft nor released")
        if template.state == "released" and template.hash != content_hash(template):
            say("its files changed after release; make the next version instead")
        if template.state == "draft" and template.hash is not None:
            say("a draft has no hash")
        texts = [template.subject, template.text, template.html]
        try:
            refs = [references(text) for text in texts]
        except TemplateError as error:
            say(str(error))
            continue
        raw = [name for _, _, names in refs for name in names]
        if template.type == "Layout":
            if template.layout:
                say("a layout has no layout")
            for label, text in (("body.txt", template.text), ("body.html", template.html)):
                if text.count("{{{ @content }}}") != 1:
                    say(f"{label} needs {{{{{{ @content }}}}}} exactly once")
            allowed = dict(template.fields)
        else:
            layout = layouts.get(template.layout or "")
            if layout is None:
                say(f"layout {template.layout!r} is not a layout in the manifest")
                continue
            if templates.index(layout) > templates.index(template):
                say("its layout must come first in the manifest")
            if not template.subject:
                say("subject.txt is empty")
            allowed = {**layout.fields, **template.fields}
        for name in raw:
            if name != "@content" or template.type != "Layout":
                say(f"raw {{{{{{ {name} }}}}}} is not allowed; use {{{{ {name} }}}}")
        used_top = set().union(*(top for top, _, _ in refs))
        used_lists: dict[str, set[str]] = {}
        for _, lists, _ in refs:
            for name, inner in lists.items():
                used_lists.setdefault(name, set()).update(inner)
        for name in sorted(used_top - set(allowed)):
            say(f"uses {name}, which is not in the manifest's fields")
        for name in sorted(set(template.fields) - used_top):
            say(f"field {name} is never used")
        for name, inner in sorted(used_lists.items()):
            kind = allowed.get(name)
            if not isinstance(kind, list):
                say(f"{{{{#each {name}}}}} needs a list field")
                continue
            declared = set(kind[0])
            for extra in sorted(inner - declared):
                say(f"{name}[] uses {extra}, which is not in its fields")
            for unused in sorted(declared - inner):
                say(f"{name}[] field {unused} is never used")
        if model_shape(template.test_model) != shape(template.fields):
            say("the test model's fields differ from the manifest's fields")
        for value in strings(template.test_model):
            if any(pattern.search(value) for pattern in SECRET_SHAPED):
                say(f"the test model holds a secret-shaped value: {value[:12]}...")
    return found


def full_model(template: Template, templates: list[Template]) -> dict[str, Any]:
    layout = next((t for t in templates if t.alias == template.layout), None)
    return {**(layout.test_model if layout else {}), **template.test_model}


Opener = Callable[..., Any]


class Postmark:
    def __init__(self, token: str, opener: Opener = urllib.request.urlopen) -> None:
        self._token = token
        self._open = opener

    def request(self, method: str, path: str, body: Any = None, missing_ok: bool = False) -> Any:
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(
            API + path,
            data=data,
            method=method,
            headers={
                "Accept": "application/json",
                "Content-Type": "application/json",
                "X-Postmark-Server-Token": self._token,
            },
        )
        try:
            with self._open(request, timeout=15) as response:
                return json.loads(response.read() or b"{}")
        except urllib.error.HTTPError as error:
            try:
                answer = json.loads(error.read() or b"{}")
            except ValueError:
                answer = {}
            code = answer.get("ErrorCode")
            if missing_ok and (error.code == 404 or code == 1101):
                return None
            if error.code in (401, 403) or code == 10:
                raise TemplateError("Postmark refused the server token in POSTMARK_SERVER_TOKEN") from None
            raise TemplateError(
                f"{method} {path}: HTTP {error.code}, ErrorCode {code}: {answer.get('Message', '')}"
            ) from None
        except urllib.error.URLError as error:
            raise TemplateError(f"{method} {path}: {error.reason}") from None

    def server(self) -> tuple[str, str]:
        """The server's name and delivery type; the rest of the answer holds
        its API tokens and is dropped here."""
        answer = self.request("GET", "/server")
        name, delivery = answer.get("Name"), answer.get("DeliveryType")
        if not isinstance(name, str) or not isinstance(delivery, str):
            raise TemplateError("GET /server answered without a Name and DeliveryType")
        return name, delivery

    def templates(self) -> list[dict[str, Any]]:
        found: list[dict[str, Any]] = []
        while True:
            answer = self.request("GET", f"/templates?count=100&offset={len(found)}&templateType=All")
            page = answer.get("Templates")
            total = answer.get("TotalCount")
            if not isinstance(page, list) or not isinstance(total, int):
                raise TemplateError("GET /templates answered without Templates and TotalCount")
            found += page
            if not page or len(found) >= total:
                return found

    def template(self, alias: str) -> dict[str, Any] | None:
        return self.request("GET", f"/templates/{alias}", missing_ok=True)


def expected_name(args: argparse.Namespace) -> str:
    name = getattr(args, "server", None) or os.environ.get("POSTMARK_SERVER_NAME", "")
    if not name:
        raise TemplateError("changes need the server's name: --server NAME or POSTMARK_SERVER_NAME")
    return name


def connect(args: argparse.Namespace, opener: Opener, out: Callable[[str], None]) -> Postmark:
    token = os.environ.get("POSTMARK_SERVER_TOKEN", "")
    if not token:
        raise TemplateError("set POSTMARK_SERVER_TOKEN to the Postmark server's API token")
    api = Postmark(token, opener)
    name, delivery = api.server()
    out(f"Postmark server {name!r} ({delivery})")
    if getattr(args, "apply", False) and name != expected_name(args):
        raise TemplateError(f"the token belongs to server {name!r}, not {expected_name(args)!r}; nothing changed")
    return api


def normalized(text: Any) -> str:
    return (text or "").replace("\r\n", "\n")


def differences(template: Template, live: dict[str, Any]) -> list[str]:
    want = template.body()
    changed = []
    for key in ("Name", "Subject", "HtmlBody", "TextBody", "TemplateType", "LayoutTemplate"):
        if key not in want and key in ("Subject", "LayoutTemplate"):
            if live.get(key):
                changed.append(key)
            continue
        if normalized(want.get(key)) != normalized(live.get(key)):
            changed.append(key)
    return changed


def validate(api: Postmark, templates: list[Template], out: Callable[[str], None]) -> bool:
    good = True
    layouts = {t.alias: t for t in templates if t.type == "Layout"}
    for template in templates:
        body: dict[str, Any] = {
            "HtmlBody": template.html,
            "TextBody": template.text,
            "TemplateType": template.type,
            "TestRenderModel": full_model(template, templates),
        }
        if template.type == "Standard":
            body["Subject"] = template.subject
            if api.template(template.layout or "") is not None:
                body["LayoutTemplate"] = template.layout
        answer = api.request("POST", "/templates/validate", body)
        errors = []
        for part in ("Subject", "HtmlBody", "TextBody"):
            for error in (answer.get(part) or {}).get("ValidationErrors") or []:
                errors.append(f"{part}: {error.get('Message')}")
        if answer.get("AllContentIsValid") is not True:
            errors.append("Postmark says the content is not valid")
        suggested = model_shape(answer.get("SuggestedTemplateModel") or {})
        own = shape(template.fields)
        allowed = dict(own)
        if template.type == "Standard" and template.layout in layouts:
            allowed |= shape(layouts[template.layout].fields)
        if not set(own) <= set(suggested) or not set(suggested) <= set(allowed):
            errors.append(f"Postmark's suggested model has {sorted(suggested)}, the manifest {sorted(own)}")
        else:
            for name, kind in suggested.items():
                if kind != allowed[name]:
                    errors.append(f"field {name} is shaped {kind} on Postmark, {allowed[name]} in the manifest")
        if errors:
            good = False
            out(f"  {template.alias}: INVALID")
            for error in errors:
                out(f"    {error}")
        else:
            out(f"  {template.alias}: valid")
    return good


def command_list(api: Postmark, templates: list[Template], out: Callable[[str], None]) -> int:
    ours = {t.alias for t in templates}
    live = api.templates()
    for item in sorted(live, key=lambda t: (t.get("TemplateType") != "Layout", t.get("Alias") or "")):
        alias = item.get("Alias") or f"(no alias, id {item.get('TemplateId')})"
        owner = "bmac" if alias in ours else ("bmac, not in this manifest" if alias.startswith(PREFIX) else "foreign")
        out(f"  {alias}  {item.get('TemplateType')}  {owner}")
    out(f"{len(live)} of {LIMIT} templates")
    return 0


def command_diff(api: Postmark, templates: list[Template], out: Callable[[str], None]) -> int:
    drift = 0
    for template in templates:
        live = api.template(template.alias)
        if live is None:
            out(f"  {template.alias}: missing")
            drift += 1
            continue
        changed = differences(template, live)
        if changed:
            drift += 1
            out(f"  {template.alias}: differs in {', '.join(changed)}")
        else:
            out(f"  {template.alias}: same")
    return 1 if drift else 0


def command_sync(
    api: Postmark, templates: list[Template], apply: bool, out: Callable[[str], None]
) -> int:
    out("Validating:")
    if not validate(api, templates, out):
        raise TemplateError("validation failed; nothing changed")
    live = api.templates()
    plan: list[tuple[str, Template]] = []
    drift = []
    for template in templates:
        current = api.template(template.alias)
        if current is None:
            plan.append(("create", template))
        elif current.get("TemplateType") != template.type:
            drift.append(f"{template.alias} is a {current.get('TemplateType')} on the server")
        elif differences(template, current):
            if template.state == "released":
                drift.append(f"{template.alias} is released but differs on the server")
            else:
                plan.append(("update", template))
    if drift:
        for line in drift:
            out(f"  DRIFT {line}")
        raise TemplateError("released templates are never overwritten; nothing changed")
    creates = sum(1 for action, _ in plan if action == "create")
    if len(live) + creates > LIMIT:
        raise TemplateError(f"{len(live)} + {creates} templates would exceed Postmark's {LIMIT}; retire old versions")
    if not plan:
        out("Nothing to change.")
        return 0
    for action, template in plan:
        out(f"  {action} {template.alias}")
    if not apply:
        out("Plan only; run again with --apply to make these changes.")
        return 0
    order = sorted(plan, key=lambda item: item[1].type != "Layout")
    for action, template in order:
        if action == "create":
            answer = api.request("POST", "/templates", template.body())
        else:
            answer = api.request("PUT", f"/templates/{template.alias}", template.body())
        if answer.get("Alias") != template.alias:
            raise TemplateError(f"{action} {template.alias}: unexpected answer; stopping")
        out(f"  {action}d {template.alias}")
    return 0


def command_retire(api: Postmark, templates: list[Template], alias: str, out: Callable[[str], None]) -> int:
    if not alias.startswith(PREFIX):
        raise TemplateError(f"{alias} is not BMAC's; only {PREFIX}* templates are retired")
    if alias in {t.alias for t in templates}:
        raise TemplateError(f"{alias} is in this release's manifest; it is still in use")
    live = api.templates()
    if not any(t.get("Alias") == alias for t in live):
        raise TemplateError(f"{alias} is not on the server")
    users = [t.get("Alias") for t in live if t.get("LayoutTemplate") == alias]
    if users:
        raise TemplateError(f"{alias} is the layout of {', '.join(map(str, users))}; retire those first")
    api.request("DELETE", f"/templates/{alias}")
    out(f"  retired {alias}")
    return 0


def command_send(
    api: Postmark, templates: list[Template], args: argparse.Namespace, out: Callable[[str], None]
) -> int:
    template = next((t for t in templates if t.alias == args.alias), None)
    if template is None or template.type != "Standard":
        raise TemplateError(f"{args.alias} is not a standard template in the manifest")
    answer = api.request(
        "POST",
        "/email/withTemplate",
        {
            "From": args.sender,
            "To": args.to,
            "TemplateAlias": template.alias,
            "TemplateModel": full_model(template, templates),
            "MessageStream": "outbound",
            "Tag": "bmac-template-test",
            "TrackOpens": False,
            "TrackLinks": "None",
        },
    )
    if answer.get("ErrorCode") != 0 or not answer.get("MessageID"):
        raise TemplateError(f"send failed: ErrorCode {answer.get('ErrorCode')}: {answer.get('Message')}")
    out(f"  sent {template.alias} to {args.to}: MessageID {answer['MessageID']}")
    return 0


def command_release(alias: str, root: Path, out: Callable[[str], None]) -> int:
    templates = load(root)
    found = problems(templates)
    if found:
        raise TemplateError("fix the manifest first:\n" + "\n".join(found))
    manifest_path = root / "manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    template = next((t for t in templates if t.alias == alias), None)
    entry = next((e for e in manifest["templates"] if e["alias"] == alias), None)
    if template is None or entry is None:
        raise TemplateError(f"{alias} is not in the manifest")
    if entry["state"] == "released":
        raise TemplateError(f"{alias} is already released")
    entry["state"] = "released"
    entry["hash"] = content_hash(template)
    manifest_path.write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    out(f"  released {alias}; its files must not change from now on")
    return 0


def parser() -> argparse.ArgumentParser:
    top = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    server = argparse.ArgumentParser(add_help=False)
    server.add_argument("--server", help="the Postmark server name that --apply must match")
    commands = top.add_subparsers(dest="command", required=True)
    commands.add_parser("check")
    commands.add_parser("list", parents=[server])
    commands.add_parser("diff", parents=[server])
    commands.add_parser("validate", parents=[server])
    sync = commands.add_parser("sync", parents=[server])
    sync.add_argument("--apply", action="store_true")
    retire = commands.add_parser("retire", parents=[server])
    retire.add_argument("alias")
    retire.add_argument("--apply", action="store_true")
    send = commands.add_parser("send", parents=[server])
    send.add_argument("alias")
    send.add_argument("--to", required=True)
    send.add_argument("--from", dest="sender", help="a confirmed Sender Signature; defaults to POSTMARK_SENDER")
    send.add_argument("--apply", action="store_true")
    release = commands.add_parser("release")
    release.add_argument("alias")
    return top


def main(
    argv: list[str] | None = None,
    opener: Opener = urllib.request.urlopen,
    root: Path = TEMPLATES,
    out: Callable[[str], None] = print,
) -> int:
    args = parser().parse_args(argv)
    try:
        if args.command == "release":
            return command_release(args.alias, root, out)
        templates = load(root)
        found = problems(templates)
        if found:
            for line in found:
                out(f"  {line}")
            raise TemplateError(f"{len(found)} problem(s) in {root}")
        if args.command == "check":
            out(f"{len(templates)} templates, no problems")
            return 0
        if args.command in ("retire", "send") and not args.apply:
            raise TemplateError(f"{args.command} changes the server; add --apply")
        if args.command == "send":
            args.sender = args.sender or os.environ.get("POSTMARK_SENDER", "").strip()
            if not args.sender:
                raise TemplateError("send needs --from or POSTMARK_SENDER: a confirmed Sender Signature")
        api = connect(args, opener, out)
        if args.command == "list":
            return command_list(api, templates, out)
        if args.command == "diff":
            return command_diff(api, templates, out)
        if args.command == "validate":
            return 0 if validate(api, templates, out) else 1
        if args.command == "sync":
            return command_sync(api, templates, args.apply, out)
        if args.command == "retire":
            return command_retire(api, templates, args.alias, out)
        return command_send(api, templates, args, out)
    except TemplateError as error:
        out(f"ERROR: {error}")
        return 1


if __name__ == "__main__":
    sys.exit(main())
