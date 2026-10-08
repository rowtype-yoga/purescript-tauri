-- | POSIX shared-memory RGBA frames backed by the native Metal canvas renderer.
module Tauri.Native.SharedFrame
  ( Frame
  , open
  , capture
  , info
  , release
  , close
  ) where

import Prelude (Unit)

import Effect (Effect)
import Tauri.Native.Canvas (Surface)

foreign import data Frame :: Type

-- | Open frame ownership and verify Metal without interacting with a terminal.
-- | Canvas allocation selects its backend explicitly.
foreign import open :: Effect Unit

-- | Read a canvas's straight RGBA pixels directly into an owned shared mapping.
foreign import capture :: Surface -> Effect Frame

-- | Metadata for an open frame. `name` is the POSIX name, including its leading
-- | slash, and `byteLength` is the RGBA mapping length.
foreign import info
  :: Frame
  -> { id :: Int, name :: String, width :: Int, height :: Int, byteLength :: Int }

-- | Release this frame's mapping. Releasing an already released frame is safe.
foreign import release :: Frame -> Effect Unit

-- | Release every remaining frame. Existing canvases retain their own backend.
foreign import close :: Effect Unit
