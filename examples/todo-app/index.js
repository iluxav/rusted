// A complete interactive web app in one file: Express-style routes, the
// account database, and htmx on the page — no build step, no framework,
// no separate frontend. Push it and the URL is the app.
//
// The pattern to notice: every mutation returns the SAME fragment renderer
// the full page uses (todoLi), so the server is the single source of truth
// and the DOM is just its reflection.

export const config = { db: true };

const ensureTable = (context) =>
  context.db.exec(
    "CREATE TABLE IF NOT EXISTS todos (id INTEGER PRIMARY KEY, title TEXT NOT NULL, done INTEGER DEFAULT 0)",
  );

// One todo → one <li>. Buttons carry their own htmx behavior, so a freshly
// swapped-in row is interactive the moment it lands.
const escapeHtml = (s) =>
  String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

const todoLi = (t) => `
  <li id="todo-${t.id}" ${t.done ? 'class="done"' : ""}>
    <span>${t.done ? `<s>${escapeHtml(t.title)}</s>` : escapeHtml(t.title)}</span>
    <span>
      <button hx-post="todo-app/todos/${t.id}/toggle" hx-target="#todo-${t.id}" hx-swap="outerHTML">
        ${t.done ? "undo" : "done"}
      </button>
      <button hx-delete="todo-app/todos/${t.id}" hx-target="#todo-${t.id}" hx-swap="outerHTML">×</button>
    </span>
  </li>`;

const page = (todos) => `<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <title>Todos</title>
  <script src="https://unpkg.com/htmx.org@2"></script>
  <style>
    body { font: 16px/1.5 system-ui; max-width: 34rem; margin: 3rem auto; padding: 0 1rem; }
    ul { list-style: none; padding: 0; }
    li { display: flex; justify-content: space-between; padding: .5rem 0; border-bottom: 1px solid #eee; }
    form { display: flex; gap: .5rem; margin-bottom: 1rem; }
    input { flex: 1; padding: .5rem; }
  </style>
</head>
<body>
  <h1>Todos</h1>
  <form hx-post="todo-app/todos" hx-target="#list" hx-swap="beforeend"
        hx-on::after-request="this.reset()">
    <input name="title" placeholder="What needs doing?" required>
    <button>Add</button>
  </form>
  <ul id="list">${todos.map(todoLi).join("")}</ul>
</body>
</html>`;

const html = (context, body, init) =>
  context.text(body, { ...init, headers: { "content-type": "text/html", ...(init && init.headers) } });

export const app = rusted
  .app({ name: "todo-app", access: "public" })
  // Middleware runs before every matched route — one CREATE TABLE here
  // beats repeating it in four handlers.
  .use(async (request, context, next) => {
    await ensureTable(context);
    return next();
  })
  .get("/", async (request, context) => {
    const todos = await context.db.query("SELECT * FROM todos ORDER BY id");
    return html(context, page(todos));
  })
  .post("/todos", async (request, context) => {
    // htmx submits forms url-encoded; URLSearchParams is a native global.
    const title = new URLSearchParams(request.body).get("title");
    if (!title) return context.json({ error: "title required" }, { status: 400 });
    const { lastInsertRowid } = await context.db.exec(
      "INSERT INTO todos (title) VALUES (?)", [title]);
    const [todo] = await context.db.query("SELECT * FROM todos WHERE id = ?", [lastInsertRowid]);
    return html(context, todoLi(todo));
  })
  .post("/todos/{id}/toggle", async (request, context) => {
    await context.db.exec(
      "UPDATE todos SET done = 1 - done WHERE id = ?", [request.params.id]);
    const [todo] = await context.db.query(
      "SELECT * FROM todos WHERE id = ?", [request.params.id]);
    if (!todo) return context.json({ error: "gone" }, { status: 404 });
    return html(context, todoLi(todo));
  })
  .delete("/todos/{id}", async (request, context) => {
    await context.db.exec("DELETE FROM todos WHERE id = ?", [request.params.id]);
    // Empty body over outerHTML removes the element. Feature complete.
    return html(context, "");
  });
