-- | Narrow bindings to the official Tauri 2 APIs. Importing these modules is
-- | safe in a normal browser; native operations still require a Tauri host.
-- | The application must register the Rust dialog/fs plugins and grant only
-- | the command permissions and file scopes it needs. No capability is added
-- | or bypassed by this library.
module Tauri (isTauri) where

import Effect (Effect)

-- | Query the official host flag before selecting a native application host.
foreign import isTauri :: Effect Boolean
