const host = () => {
  const native = globalThis.__purescriptNativeHost;
  if (!native) throw new Error("Tauri.Native requires tauri-native-host");
  return native;
};

const nativeArguments = () => host().call("arguments");
export { nativeArguments as arguments };
export const version = () => host().call("version");
export const isStdinTTY = () => host().call("isStdinTTY");
export const readStdin = () => host().call("readStdin");
export const readText = path => () => host().call("readText", path);
export const writeText = path => text => () => { host().call("writeText", path, text); };
export const stdout = text => () => { host().call("stdout", text); };
export const stderr = text => () => { host().call("stderr", text); };
export const absolutePath = path => () => host().call("absolutePath", path);
export const environment = name => () => host().call("environment", name);
export const monotonicMilliseconds = () => host().call("monotonicMilliseconds");
export const runApplicationImpl = args => input => () => host().call("runApplication", args, input);
export const measureText = font => text => () => host().call("measureText", font, text);
export const beginVideo = options => () => host().call("beginVideo", options);
export const writeVideoFrame = video => surface => () => { host().call("writeVideoFrame", video, surface); };
export const finishVideo = video => () => { host().call("finishVideo", video); };
export const cancelVideo = video => () => { host().call("cancelVideo", video); };
export const enterTerminal = () => { host().call("enterTerminal"); };
export const restoreTerminal = () => { host().call("restoreTerminal"); };
export const terminalSize = () => host().call("terminalSize");
export const readKey = () => host().call("readKey");
export const sleepMilliseconds = milliseconds => () => { host().call("sleepMilliseconds", milliseconds); };
