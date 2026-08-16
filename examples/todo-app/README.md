# Todo app

A complete interactive web app in one file: Express-style routes
(`rusted.app(...)`), the account database (`config.db`), and htmx in the
page. Push it and the URL is the app — no build step, no separate frontend.

```bash
rusted push index.js
# open https://rusted.sh/f/todo-app   (or your server's URL)
```

What it demonstrates:

- **Routing**: `.get("/", …)`, `.post("/todos", …)`,
  `.post("/todos/{id}/toggle", …)`, `.delete("/todos/{id}", …)` — captures
  arrive in `request.params`, unmatched paths 404 from the dispatcher.
- **Middleware**: one `use()` ensures the table exists before every route.
- **The htmx pattern**: the server renders HTML fragments; every mutation
  returns the same `todoLi` renderer the full page uses, so add/toggle/
  delete swap real server truth into the DOM. Note the relative
  `hx-post="todo-app/todos"` URLs — the page lives at `/f/todo-app`
  (no trailing slash), so relative paths resolve from `/f/`.
- **Form posts**: htmx submits url-encoded; `new URLSearchParams(request.body)`
  reads it — a native global, no imports.
- **The database**: plain parameterized SQL, shared with the console's
  Database tab where you can watch the rows change.

`rusted run` does not serve app modules yet — develop against a local
`rusted serve` or the console editor.
