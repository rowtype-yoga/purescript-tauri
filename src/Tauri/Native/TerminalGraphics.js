const host = () => {
  const native = globalThis.__purescriptNativeHost;
  if (!native) throw new Error("Tauri.Native.TerminalGraphics requires tauri-native-host");
  return native;
};

export const open = () => { host().call("terminalGraphicsOpen"); };
export const size = () => host().call("terminalGraphicsSize");
export const present = surface => position => () => {
  host().call("terminalGraphicsPresent", surface, position.column, position.row);
};
export const readKey = () => host().call("terminalGraphicsReadKey");
export const close = () => { host().call("terminalGraphicsClose"); };
