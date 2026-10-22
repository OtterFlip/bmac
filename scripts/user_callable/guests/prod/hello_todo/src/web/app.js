// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The todo page: everything goes through the JSON API under /api/v1/.

const $ = (id) => document.getElementById(id);

async function api(method, path, body) {
  const response = await fetch(`/api/v1${path}`, {
    method,
    headers: body ? { "Content-Type": "application/json" } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  if (response.status === 204) return null;
  const data = await response.json().catch(() => ({ error: `HTTP ${response.status}` }));
  if (!response.ok) throw new Error(data.error || `HTTP ${response.status}`);
  return data;
}

function showError(error) {
  $("error").textContent = error ? String(error.message || error) : "";
  $("error").hidden = !error;
}

function render(todos) {
  const list = $("todos");
  list.replaceChildren();
  for (const todo of todos) {
    const item = document.createElement("li");
    item.className = todo.done ? "done" : "";

    const check = document.createElement("input");
    check.type = "checkbox";
    check.checked = todo.done;
    check.title = "Done";
    check.addEventListener("change", () => change(todo.id, { done: check.checked }));

    const title = document.createElement("span");
    title.className = "title";
    title.textContent = todo.title;
    title.title = "Double-click to edit";
    title.addEventListener("dblclick", () => {
      const text = prompt("Edit todo", todo.title);
      if (text !== null && text.trim() && text !== todo.title) change(todo.id, { title: text });
    });

    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "remove";
    remove.textContent = "×";
    remove.title = "Delete";
    remove.addEventListener("click", () => run(() => api("DELETE", `/todos/${todo.id}`)));

    item.append(check, title, remove);
    list.append(item);
  }
  $("count").textContent = todos.length;
  $("empty").hidden = todos.length > 0;
}

async function refresh() {
  render(await api("GET", "/todos"));
}

async function run(action) {
  try {
    await action();
    showError(null);
  } catch (error) {
    showError(error);
  }
  await refresh().catch(showError);
}

function change(id, fields) {
  return run(() => api("PATCH", `/todos/${id}`, fields));
}

$("add").addEventListener("submit", (event) => {
  event.preventDefault();
  const input = $("title");
  const title = input.value;
  run(async () => {
    await api("POST", "/todos", { title });
    input.value = "";
  });
});

api("GET", "/info")
  .then((info) => {
    $("hostname").textContent = info.hostname;
    $("limit").textContent = info.max_todos;
  })
  .catch(showError);
refresh().catch(showError);
