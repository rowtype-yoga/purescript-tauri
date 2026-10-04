-- | Official Tauri app controls. `exit` requires @tauri-apps/api 2.12.0 or later,
-- | core:app:allow-exit, and core:app:allow-set-dock-visibility as applicable.
module Tauri.App (exit, setDockVisibility) where

import Prelude (Unit)

import Effect (Effect)
import Effect.Aff (Aff)
import Promise (Promise)
import Promise.Aff (toAffE)

-- | Terminate the native application with the supplied process exit code.
exit :: Int -> Aff Unit
exit code = toAffE (exitImpl code)

-- | Show or hide the Dock icon. This is supported by Tauri on macOS only.
setDockVisibility :: Boolean -> Aff Unit
setDockVisibility visible = toAffE (setDockVisibilityImpl visible)

foreign import exitImpl :: Int -> Effect (Promise Unit)
foreign import setDockVisibilityImpl :: Boolean -> Effect (Promise Unit)
