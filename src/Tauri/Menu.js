import { Menu, MenuItem, PredefinedMenuItem, Submenu } from "@tauri-apps/api/menu";

export const itemImpl = options => action => () =>
  MenuItem.new({ ...options, action: id => action(id)() });

export const predefinedImpl = item => () => PredefinedMenuItem.new({ item });

export const aboutImpl = item => () => PredefinedMenuItem.new({ item });

export const submenuImpl = text => items => () => Submenu.new({ text, items });

export const menuImpl = items => () => Menu.new({ items });

export const setAsAppMenuImpl = menu => () => menu.setAsAppMenu();

export const closeImpl = resource => () => resource.close();
