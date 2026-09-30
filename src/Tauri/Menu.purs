-- | Generic native app menus. Requires core:menu:allow-new,
-- | core:menu:allow-set-as-app-menu and core:resources:allow-close.
-- | On macOS the top-level entries must be submenus. Item IDs must be unique
-- | within the application's menu event namespace.
module Tauri.Menu (MenuEntry, item, predefined, submenu, setAppMenu) where

import Prelude

import Data.Array as Array
import Data.Either (either)
import Data.Foldable (traverse_)
import Data.Maybe (Maybe(..))
import Data.Nullable (Nullable, null, toMaybe)
import Data.Traversable (traverse)
import Effect (Effect)
import Effect.Aff (Aff, attempt, bracket, finally, generalBracket, launchAff_, throwError)
import Effect.Class (liftEffect)
import Effect.Ref (Ref)
import Effect.Ref as Ref
import Promise (Promise)
import Promise.Aff (toAffE)
import Tauri.Internal.Resource (once)

type ItemOptions =
  { id :: String
  , text :: String
  , accelerator :: Maybe String
  , enabled :: Boolean
  }

-- | Opaque reusable descriptions, not orphaned native resource handles.
-- | Native creation happens at setAppMenu, which owns the complete tree and
-- | reports any native construction errors. Dropping an unused description
-- | requires no native cleanup.
data MenuEntry
  = Item ItemOptions (String -> Effect Unit)
  | Predefined String
  | Submenu String (Array MenuEntry)

item :: ItemOptions -> (String -> Effect Unit) -> Aff MenuEntry
item options action = pure $ Item options action

-- | Names are the official Tauri predefined variants, including "About"
-- | (uses the host application's metadata). Unsupported names fail during
-- | installation. OS-specific behavior is unchanged from Tauri; e.g. native
-- | Undo/Redo are macOS-only. Use item for application-owned commands.
predefined :: String -> Aff MenuEntry
predefined = pure <<< Predefined

submenu :: String -> Array MenuEntry -> Aff MenuEntry
submenu text entries = pure $ Submenu text entries

-- | Install the tree and retain the previous app menu until disposal.
-- | The idempotent disposer restores that menu, then closes every owned
-- | resource, including the handle returned by the restoration call.
-- |
-- | Tauri's official JS API has no remove-app-menu operation. If no previous
-- | menu existed, disposal installs an empty native menu instead. This is
-- | intentionally not a claim to restore the platform's absent-menu state.
-- |
-- | Installations must be disposed in reverse order. The Effect starts
-- | asynchronous teardown; failures surface via launchAff_ instead of being
-- | ignored. Cancelling installation releases resources once the native
-- | operations settle. The Rust commands cannot themselves be aborted.
setAppMenu :: Array MenuEntry -> Aff (Effect Unit)
setAppMenu entries = generalBracket
  (install entries)
  { killed: \_ dispose -> dispose
  , failed: \_ dispose -> dispose
  , completed: \_ _ -> pure unit
  }
  (\dispose -> liftEffect $ once $ launchAff_ dispose)

install :: Array MenuEntry -> Aff (Aff Unit)
install entries = generalBracket
  (liftEffect $ Ref.new [])
  { killed: \_ resources -> closeOwned resources
  , failed: \_ resources -> closeOwned resources
  , completed: \_ _ -> pure unit
  }
  \resources -> do
    let
      own create = do
        resource <- toAffE create
        liftEffect $ Ref.modify_ (Array.cons resource) resources
        pure resource
    nativeEntries <- traverse (materialize own) entries
    menu <- own $ menuImpl nativeEntries
    previous <- toMaybe <$> toAffE (setAsAppMenuImpl menu)
    pure $ finally (closeOwned resources) $ restore previous

materialize
  :: (Effect (Promise NativeMenuEntry) -> Aff NativeMenuEntry)
  -> MenuEntry
  -> Aff NativeMenuEntry
materialize own = case _ of
  Item options action -> case options.accelerator of
    Nothing -> own $ itemImpl
      { id: options.id, text: options.text, enabled: options.enabled }
      action
    Just accelerator -> own $ itemImpl
      { id: options.id, text: options.text, enabled: options.enabled, accelerator }
      action
  Predefined "About" -> own $ aboutImpl { "About": null }
  Predefined name -> own $ predefinedImpl name
  Submenu text entries -> do
    children <- traverse (materialize own) entries
    own $ submenuImpl text children

restore :: Maybe NativeMenuEntry -> Aff Unit
restore previous = bracket
  (case previous of
    Just menu -> pure menu
    Nothing -> toAffE $ menuImpl []
  )
  (toAffE <<< closeImpl)
  (\menu -> do
    displaced <- toMaybe <$> toAffE (setAsAppMenuImpl menu)
    traverse_ (toAffE <<< closeImpl) displaced
  )

-- Attempt every close even if an earlier native resource fails to close.
-- All failures remain failures; report the first after attempting the rest.
closeOwned :: Ref (Array NativeMenuEntry) -> Aff Unit
closeOwned resources = do
  owned <- liftEffect $ Ref.read resources
  results <- traverse (attempt <<< toAffE <<< closeImpl) owned
  traverse_ (either throwError pure) results

foreign import data NativeMenuEntry :: Type
foreign import itemImpl
  :: forall r
   . { id :: String, text :: String, enabled :: Boolean | r }
  -> (String -> Effect Unit)
  -> Effect (Promise NativeMenuEntry)
foreign import predefinedImpl :: String -> Effect (Promise NativeMenuEntry)
foreign import aboutImpl :: { "About" :: Nullable Unit } -> Effect (Promise NativeMenuEntry)
foreign import submenuImpl :: String -> Array NativeMenuEntry -> Effect (Promise NativeMenuEntry)
foreign import menuImpl :: Array NativeMenuEntry -> Effect (Promise NativeMenuEntry)
foreign import setAsAppMenuImpl :: NativeMenuEntry -> Effect (Promise (Nullable NativeMenuEntry))
foreign import closeImpl :: NativeMenuEntry -> Effect (Promise Unit)
