-- | Reusable Skia canvases. Requires tauri-native-host.
-- | Normally CPU-backed; canvases created during a macOS terminal graphics
-- | session use Metal instead.
-- | Surface handles must be released when their owner is finished drawing.
module Tauri.Native.Canvas
  ( Surface
  , Color
  , Affine
  , Rect
  , Path
  , Stroke
  , Text
  , CirclePattern
  , create
  , release
  , reset
  , clear
  , save
  , restore
  , setTransform
  , transform
  , clip
  , setBlend
  , setBlur
  , setTint
  , fillPath
  , strokePath
  , fillStrokePath
  , text
  , circlePattern
  , surface
  , writePng
  ) where

import Prelude

import Data.Maybe (Maybe)
import Data.Nullable (Nullable, toNullable)
import Effect (Effect)

-- | Straight (unpremultiplied) RGBA channels in 0..255.
type Color = { r :: Int, g :: Int, b :: Int, a :: Int }

type Affine = { a :: Number, b :: Number, c :: Number, d :: Number, e :: Number, f :: Number }

type Rect = { x :: Number, y :: Number, width :: Number, height :: Number }

-- | A range in a packed path array: 1 move(x,y), 2 line(x,y),
-- | 3 quad(cx,cy,x,y), 4 cubic(cx1,cy1,cx2,cy2,x,y), 5 close.
type Path = { values :: Array Number, offset :: Int, length :: Int }

-- | Joins: 0 round, 1 bevel, 2 miter. Caps: 0 butt, 1 round, 2 square.
type Stroke = { color :: Color, width :: Number, join :: Int, cap :: Int }

-- | CSS shorthand font, shared with Tauri.Native.measureText.
-- | Align: 0 left, 1 center, 2 right. Baseline: 0 top, 1 middle,
-- | 2 alphabetic, 3 bottom. Baselines use font metrics, not each word's ink.
-- | Registered OpenType features are shared by shaping and positioned drawing.
type Text =
  { x :: Number
  , y :: Number
  , content :: String
  , font :: String
  , color :: Color
  , align :: Int
  , baseline :: Int
  }

type CirclePattern =
  { rect :: Rect
  , background :: Color
  , ink :: Color
  , tile :: Number
  , radius :: Number
  , originX :: Number
  , originY :: Number
  }

-- | Apply a SrcIn color-filter tint; Nothing removes the filter.
setTint :: Surface -> Maybe Color -> Effect Unit
setTint target color = setTintImpl target (toNullable color)

foreign import data Surface :: Type

foreign import create :: { width :: Int, height :: Int } -> Effect Surface
foreign import release :: Surface -> Effect Unit
-- | Clear transparent and reset transform, clipping, styles and save stack.
foreign import reset :: Surface -> Effect Unit
-- | Ignore the transform while respecting the current clip.
foreign import clear :: Surface -> Color -> Effect Unit
foreign import save :: Surface -> Effect Unit
foreign import restore :: Surface -> Effect Unit
foreign import setTransform :: Surface -> Affine -> Effect Unit
foreign import transform :: Surface -> Affine -> Effect Unit
-- | True selects the even-odd fill rule.
foreign import clip :: Surface -> Path -> Boolean -> Effect Unit
-- | source-over, difference, destination-out, destination-in, source-in or copy.
foreign import setBlend :: Surface -> String -> Effect Unit
-- | Filter radius in display pixels, unaffected by the current transform.
foreign import setBlur :: Surface -> Number -> Effect Unit
foreign import setTintImpl :: Surface -> Nullable Color -> Effect Unit
foreign import fillPath :: Surface -> Path -> Color -> Effect Unit
foreign import strokePath :: Surface -> Path -> Stroke -> Effect Unit
foreign import fillStrokePath :: Surface -> Path -> Color -> Stroke -> Effect Unit
foreign import text :: Surface -> Text -> Effect Unit
foreign import circlePattern :: Surface -> CirclePattern -> Effect Unit
-- | Destination then source; draw at 0,0 using destination transform and style.
foreign import surface :: Surface -> Surface -> Effect Unit
foreign import writePng :: Surface -> String -> Effect Unit
