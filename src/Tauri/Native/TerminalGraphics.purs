-- | Kitty graphics terminal session backed by tauri-native-host.
-- | Call `close` exactly once after `open`, including exceptional exits.
module Tauri.Native.TerminalGraphics
  ( open
  , size
  , present
  , readKey
  , close
  ) where

import Prelude (Unit)

import Effect (Effect)
import Tauri.Native.Canvas (Surface)

-- | Require a usable TTY, verify Kitty graphics support, then acquire a
-- | fullscreen terminal session. Throws when the protocol is unavailable.
foreign import open :: Effect Unit

-- | Current terminal cell and pixel dimensions. Pixel values are obtained from
-- | the terminal, never inferred from a cell-size guess.
foreign import size :: Effect { columns :: Int, rows :: Int, width :: Int, height :: Int }

-- | Display a canvas as a native-size PNG at a one-based terminal cell position.
-- | The session owns and replaces the image placement on every call.
foreign import present :: Surface -> { column :: Int, row :: Int } -> Effect Unit

-- | Read one normalized key token without consuming terminal protocol replies.
foreign import readKey :: Effect String

-- | Delete the session image and restore the terminal. This is idempotent.
foreign import close :: Effect Unit
