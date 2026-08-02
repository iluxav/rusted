/// <reference path="./rusted.d.ts" />

interface Input {
  name?: string;
}

export const config: Rusted.Config = { name: "test", methods: ["POST"] };

const handler: Rusted.Handler = async (request, context) => {
  const { name } = await request.json<Input>();

  return context.json({ message: `Hello, ${name ?? "world"}` });
};

export default handler;
