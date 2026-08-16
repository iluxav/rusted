/// <reference path="./rusted.d.ts" />

interface Input {
  name?: string;
}

export const app = rusted
  .app({ name: "test" })
  .post("/", greet);

async function greet(request: Rusted.Request, context: Rusted.Context) {
  const { name } = await request.json<Input>().catch((): Input => ({}));
  return context.json({ message: `Hello, ${name ?? "world"}` });
}
