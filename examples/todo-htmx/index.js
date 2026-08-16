// The todo app again, with the HTML where designers expect it: in .html
// files. Templates import as strings (the bundler inlines them — the deployed
// artifact is still one file) and render server-side with minijinja via
// context.render. Auto-escaping is always on, so there is no escapeHtml
// helper anywhere in this project.
//
// One interpolation owner per file: `{{ expr }}` belongs to the templates,
// `${expr}` to JavaScript. This file has neither — that's the point.

import pageTpl from "./templates/page.html";
import itemTpl from "./templates/todo-item.html";

export const config = { db: true };

const item = (context, todo) => context.render(itemTpl, todo);

export const app = rusted
  .app({ name: "todo-htmx", access: "public" })
  .use(async (request, context, next) => {
    await context.db.exec(
      "CREATE TABLE IF NOT EXISTS todos (id INTEGER PRIMARY KEY, title TEXT NOT NULL, done INTEGER DEFAULT 0)",
    );
    return next();
  })
  .get("/", async (request, context) => {
    const todos = await context.db.query("SELECT * FROM todos ORDER BY id");
    // Each row renders through the same template every mutation returns;
    // the joined result is already-escaped HTML, so the page marks it |safe.
    const items = todos.map((todo) => item(context, todo)).join("");
    return context.html(context.render(pageTpl, { items }));
  })
  .post("/todos", async (request, context) => {
    // htmx submits forms url-encoded; URLSearchParams is a native global.
    const title = new URLSearchParams(request.body).get("title");
    if (!title) return context.json({ error: "title required" }, { status: 400 });
    const { lastInsertRowid } = await context.db.exec(
      "INSERT INTO todos (title) VALUES (?)", [title]);
    const [todo] = await context.db.query("SELECT * FROM todos WHERE id = ?", [lastInsertRowid]);
    return context.html(item(context, todo));
  })
  .post("/todos/{id}/toggle", async (request, context) => {
    await context.db.exec(
      "UPDATE todos SET done = 1 - done WHERE id = ?", [request.params.id]);
    const [todo] = await context.db.query(
      "SELECT * FROM todos WHERE id = ?", [request.params.id]);
    if (!todo) return context.json({ error: "gone" }, { status: 404 });
    return context.html(item(context, todo));
  })
  .delete("/todos/{id}", async (request, context) => {
    await context.db.exec("DELETE FROM todos WHERE id = ?", [request.params.id]);
    // Empty body over outerHTML removes the element.
    return context.html("");
  });
