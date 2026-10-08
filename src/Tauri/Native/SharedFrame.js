const host = () => {
  const native = globalThis.__purescriptNativeHost;
  if (!native) throw new Error("Tauri.Native.SharedFrame requires tauri-native-host");
  return native;
};

export const open = () => { host().call("sharedFrameOpen"); };
export const capture = surface => () => host().call("sharedFrameCapture", surface);
export const info = frame => host().call("sharedFrameInfo", frame);
export const consumed = frame => () => host().call("sharedFrameConsumed", frame);
export const release = frame => () => { host().call("sharedFrameRelease", frame); };
export const close = () => { host().call("sharedFrameClose"); };
