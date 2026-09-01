export default function registerE2EReloadCommand(pi) {
  pi.registerCommand("e2e-reload-extensions", {
    description: "Reload Pi resources for the real-process extension lifecycle fixture",
    async handler(_args, ctx) {
      await ctx.reload();
      return;
    },
  });
}
