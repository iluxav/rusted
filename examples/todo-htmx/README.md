# Todo app, templated

The same app as [todo-app](../todo-app), restructured the way a designer
would want it: HTML lives in `.html` files under `templates/`, JavaScript
holds only routes and SQL.

```
todo-htmx/
  index.js                    routes, middleware, queries — no HTML strings
  templates/page.html         the full page
  templates/todo-item.html    one <li>; every mutation returns it
```

Two features carry the shape:

- **`.html` imports** — `import pageTpl from "./templates/page.html"` bundles
  the file in as a string. The deployed artifact is still one file; `rusted
  pull` shows everything.
- **`context.render(template, data)`** — minijinja server-side: `{{ title }}`,
  `{% if %}`, `{% for %}`, filters. HTML auto-escaping is always on, which is
  why this project has no `escapeHtml` helper; the one place composed HTML
  enters a template (`{{ items|safe }}` in the page) says so explicitly.

Each file has one interpolation owner: `{{ }}` is minijinja in templates,
`${ }` is JavaScript in modules. Neither evaluates the other's syntax, so
there's no double-interpolation to reason about.

```bash
rusted run index.js     # scratch database locally, resets on exit
rusted push index.js    # deploy: bundles js + templates into one artifact
```
