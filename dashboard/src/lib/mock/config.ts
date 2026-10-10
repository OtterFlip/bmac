// The browser preview's config directory: the repository's real examples plus
// operator files made from them, kept in memory. Add ?fresh to the preview URL
// to start as a fresh install whose files are still unchanged examples.

import clusterExample from "@config-examples/cluster_dot_conf?raw";
import mox1Example from "@config-examples/mox1_dot_conf?raw";
import mox2Example from "@config-examples/mox2_dot_conf?raw";
import secretsExample from "@config-examples/secrets_dot_env?raw";
import type { ConfigFile, ConfigFileKind, ConfigListing, ConfigText, PreflightCheck } from "@/protocol/types";

const ESSENTIAL = ["cluster.conf", "mox1.conf", "mox2.conf", "secrets.env"];
const DEFAULT_DIR = "/home/operator/.config/com.btvcorp.bmac.dashboard/config";

const fail = (message: string) => ({ code: "invalid_argument", message });

const kindOf = (name: string): ConfigFileKind | null =>
  name.includes("_dot_") ? "example" : name.endsWith(".env") ? "secret" : name.endsWith(".conf") ? "config" : null;
const exampleFor = (name: string) => name.replace(/\.([^.]+)$/, "_dot_$1");
const userFileFor = (example: string) => example.replace(/_dot_([^_]+)$/, ".$1");

function revision(text: string) {
  let hash = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) hash = Math.imul(hash ^ text.charCodeAt(i), 0x01000193) >>> 0;
  return `${hash.toString(16)}-${text.length}`;
}

function customize(example: string, edits: Record<string, string>) {
  return example
    .split("\n")
    .map((line) => {
      const key = line.split("=")[0];
      return key in edits && !line.trimStart().startsWith("#") ? `${key}=${edits[key]}` : line;
    })
    .join("\n");
}

export function createMockConfig(fresh: boolean) {
  const examples: Record<string, string> = {
    cluster_dot_conf: clusterExample,
    mox1_dot_conf: mox1Example,
    mox2_dot_conf: mox2Example,
    secrets_dot_env: secretsExample,
  };
  const files = new Map<string, { text: string; modified: string }>();
  const now = () => new Date().toISOString();
  const put = (name: string, text: string) => files.set(name, { text, modified: now() });
  for (const [name, text] of Object.entries(examples)) put(name, text);
  if (fresh) {
    for (const name of ESSENTIAL) put(name, examples[exampleFor(name)]);
  } else {
    put("cluster.conf", customize(clusterExample, { PROXMOX_CLUSTER_NAME: "bmac", PROXMOX_CONTROL_NODE: "mox1" }));
    put("mox1.conf", customize(mox1Example, { PROXMOX_IP: "203.0.113.11" }));
    put("mox2.conf", customize(mox2Example, { PROXMOX_IP: "203.0.113.12" }));
    put("mox3.conf", customize(mox2Example, { PROXMOX_IP: "203.0.113.13" }));
    put("secrets.env", "IDRAC_PASSWORD=not-shown\n");
  }
  let dir = DEFAULT_DIR;

  const state = (name: string) => {
    const file = files.get(name);
    if (!file) return "missing" as const;
    const example = examples[exampleFor(name)];
    if (example === undefined) return "no_example" as const;
    return file.text === example ? ("unchanged" as const) : ("customized" as const);
  };

  const describe = (name: string): ConfigFile => {
    const kind = kindOf(name)!;
    const file = files.get(name);
    return {
      name,
      kind,
      present: !!file,
      essential: ESSENTIAL.includes(name),
      example: kind === "example" ? null : examples[exampleFor(name)] !== undefined ? exampleFor(name) : null,
      user_file: kind === "example" ? userFileFor(name) : null,
      state: kind === "example" ? null : state(name),
      size: file ? file.text.length : null,
      modified: file?.modified ?? null,
    };
  };

  const byName = new Intl.Collator("en", { numeric: true, sensitivity: "base" });

  const listing = (): ConfigListing => {
    const names = new Set([...files.keys(), ...ESSENTIAL]);
    const list = [...names].filter((n) => kindOf(n)).map(describe);
    list.sort((a, b) => Number(a.kind === "example") - Number(b.kind === "example") || byName.compare(a.name, b.name));
    return { dir, default_dir: DEFAULT_DIR, relocatable: true, problem: null, files: list };
  };

  const text = (name: string, read_only: boolean): ConfigText => {
    const file = files.get(name);
    if (!file) throw fail(`${name} does not exist`);
    return { name, text: file.text, revision: revision(file.text), read_only };
  };

  return {
    listing,
    read(name: string): ConfigText {
      const kind = kindOf(name);
      if (!kind) throw fail(`${name} is not a BMAC config file name`);
      if (kind === "secret") throw fail(`The dashboard does not open ${name}; edit it in your own editor.`);
      return text(name, kind !== "config");
    },
    readExample(name: string): ConfigText {
      const example = examples[name];
      if (example === undefined || name.endsWith("_dot_env")) throw fail(`${name} is not a config example`);
      return { name, text: example, revision: revision(example), read_only: true };
    },
    write(name: string, body: string, rev: string | null): ConfigText {
      if (kindOf(name) !== "config") throw fail(`${name} cannot be edited in the dashboard`);
      const current = files.get(name);
      if ((current ? revision(current.text) : null) !== rev) {
        throw { code: "unavailable", message: `${name} changed on disk since it was opened. Reload it before saving.` };
      }
      put(name, body);
      return text(name, false);
    },
    createFromExample(example: string): string {
      const target = userFileFor(example);
      if (examples[example] === undefined) throw fail(`${example} does not exist`);
      if (files.has(target)) throw fail(`${target} already exists`);
      put(target, examples[example]);
      return target;
    },
    relocate(path: string): ConfigListing {
      if (!path.startsWith("/")) throw fail("Choose an absolute directory path.");
      if (path.replace(/\/$/, "") === dir) throw fail("That is already the config directory.");
      dir = path.replace(/\/$/, "");
      return listing();
    },
    checks(): PreflightCheck[] {
      const why: Record<string, string> = {
        "cluster.conf": "every cluster workflow needs it",
        "secrets.env": "creating hosts and VMs needs it",
        "mox1.conf": "setting up the cluster's first host needs it",
        "mox2.conf": "setting up the cluster's second host needs it",
      };
      return ESSENTIAL.map((name): PreflightCheck => {
        const s = state(name);
        const id = name.replace(".", "_");
        const example = exampleFor(name);
        if (s === "missing") {
          return { id, label: name, status: name === "cluster.conf" ? "error" : "warning", detail: `Missing; ${why[name]}.`, fix: `Create it from ${example} on the Config page.`, config_file: name };
        }
        if (s === "unchanged") {
          return { id, label: name, status: "warning", detail: `Still identical to ${example}; fill in your own values.`, fix: name.endsWith(".env") ? `Open ${name} in your editor, fill it in, and keep it private (chmod 600).` : `Edit ${name} on the Config page.`, config_file: name };
        }
        return { id, label: name, status: "ok", detail: name.endsWith(".env") ? "Customized (its contents are never shown by the dashboard)." : "Customized.", config_file: name };
      });
    },
    get dir() {
      return dir;
    },
  };
}
