import { getCurrentWindow } from "@tauri-apps/api/window";

export const setTitleImpl = title => () => getCurrentWindow().setTitle(title);

export const onCloseRequestedImpl = handler => () =>
  getCurrentWindow().onCloseRequested(event => handler(event)());

export const preventClose = event => () => event.preventDefault();

export const destroyImpl = () => getCurrentWindow().destroy();
