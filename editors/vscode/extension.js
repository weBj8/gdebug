// Thin VS Code extension: runs the native `gdebug dap` adapter. Consent (allowHookReplacement)
// must come from the user's launch configuration; it is never injected here.
const vscode = require("vscode");

function activate(context) {
  context.subscriptions.push(
    vscode.debug.registerDebugAdapterDescriptorFactory("gdebug", {
      createDebugAdapterDescriptor() {
        const path = vscode.workspace.getConfiguration("gdebug").get("adapterPath") || "gdebug";
        return new vscode.DebugAdapterExecutable(path, ["dap"]);
      },
    }),
    vscode.debug.registerDebugConfigurationProvider("gdebug", {
      resolveDebugConfiguration(_folder, config) {
        if (config.request && config.request !== "attach") {
          vscode.window.showErrorMessage("GDebug is attach-only: it never launches or terminates GMod.");
          return undefined;
        }
        return config;
      },
    })
  );
}

module.exports = { activate, deactivate() {} };
