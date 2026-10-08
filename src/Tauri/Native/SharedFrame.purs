-- | POSIX shared-memory RGBA frames backed by the native Metal canvas renderer.
module Tauri.Native.SharedFrame
  ( Frame
  , open
  , capture
  , info
  , consumed
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
-- | slash. `byteLength` is the tight RGBA length, excluding any POSIX page padding.
foreign import info
  :: Frame
  -> { id :: Int, name :: String, width :: Int, height :: Int, byteLength :: Int }

-- | Whether the terminal has unlinked this frame's POSIX name. This checks only
-- | the named object: it neither copies pixels nor releases the owned mapping.
-- | Non-missing shared-memory errors are raised.
foreign import consumed :: Frame -> Effect Boolean

-- | Release this frame's mapping. Releasing an already released frame is safe.
foreign import release :: Frame -> Effect Unit

-- | Release every remaining frame. Existing canvases retain their own backend.
foreign import close :: Effect Unit
