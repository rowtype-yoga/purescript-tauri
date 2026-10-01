const host = () => {
  const native = globalThis.__purescriptNativeHost;
  if (!native) throw new Error("Tauri.Native.Canvas requires tauri-native-host");
  return native;
};

export const create = size => () => host().call("canvasCreate", size);
export const release = target => () => { host().call("canvasRelease", target); };
export const reset = target => () => { host().call("canvasReset", target); };
export const clear = target => color => () => { host().call("canvasClear", target, color); };
export const save = target => () => { host().call("canvasSave", target); };
export const restore = target => () => { host().call("canvasRestore", target); };
export const setTransform = target => affine => () => { host().call("canvasSetTransform", target, affine); };
export const transform = target => affine => () => { host().call("canvasTransform", target, affine); };
export const clip = target => path => evenOdd => () => { host().call("canvasClip", target, path, evenOdd); };
export const setBlend = target => mode => () => { host().call("canvasSetBlend", target, mode); };
export const setBlur = target => radius => () => { host().call("canvasSetBlur", target, radius); };
export const setTintImpl = target => color => () => { host().call("canvasSetTint", target, color); };
export const fillPath = target => path => color => () => { host().call("canvasFillPath", target, path, color); };
export const strokePath = target => path => stroke => () => { host().call("canvasStrokePath", target, path, stroke); };
export const fillStrokePath = target => path => color => stroke => () => { host().call("canvasFillStrokePath", target, path, color, stroke); };
export const text = target => value => () => { host().call("canvasText", target, value); };
export const circlePattern = target => pattern => () => { host().call("canvasCirclePattern", target, pattern); };
export const surface = destination => source => () => { host().call("canvasSurface", destination, source); };
export const writePng = target => path => () => { host().call("canvasWritePng", target, path); };
