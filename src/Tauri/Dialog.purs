-- | Requires dialog:allow-open, dialog:allow-save and/or dialog:allow-message.
-- | Native dialogs cannot be aborted by cancelling their Aff; cancellation
-- | stops waiting, while a user dismissal is returned as Nothing/Cancel.
module Tauri.Dialog (Filter, Choice(..), openFile, saveFile, choose) where

import Prelude

import Data.Maybe (Maybe(..))
import Data.Nullable (Nullable, toMaybe)
import Effect (Effect)
import Effect.Aff (Aff, throwError)
import Effect.Exception (error)
import Promise (Promise)
import Promise.Aff (toAffE)

type Filter = { name :: String, extensions :: Array String }

data Choice = Yes | No | Cancel

derive instance Eq Choice

instance Show Choice where
  show Yes = "Yes"
  show No = "No"
  show Cancel = "Cancel"

openFile :: { title :: String, filters :: Array Filter } -> Aff (Maybe String)
openFile options = toMaybe <$> toAffE (openFileImpl options)

saveFile
  :: { title :: String, defaultPath :: Maybe String, filters :: Array Filter }
  -> Aff (Maybe String)
saveFile options = toMaybe <$> case options.defaultPath of
  Nothing -> toAffE $ saveFileImpl { title: options.title, filters: options.filters }
  Just defaultPath -> toAffE $ saveFileImpl
    { title: options.title, filters: options.filters, defaultPath }

-- | Custom dialogs return the button label, not a stable button identifier.
-- | Labels must therefore be distinct. Unexpected native results are errors,
-- | never an implicit discard or cancellation.
choose
  :: { title :: String, message :: String, yes :: String, no :: String, cancel :: String }
  -> Aff Choice
choose options = do
  when (options.yes == options.no || options.yes == options.cancel || options.no == options.cancel) $
    throwError $ error "Tauri.Dialog.choose requires distinct button labels"
  result <- toAffE $ chooseImpl options.message
    { title: options.title
    , buttons: { yes: options.yes, no: options.no, cancel: options.cancel }
    }
  if result == options.yes then pure Yes
  else if result == options.no then pure No
  else if result == options.cancel then pure Cancel
  else throwError $ error $ "Unexpected Tauri dialog result: " <> show result

foreign import openFileImpl
  :: { title :: String, filters :: Array Filter }
  -> Effect (Promise (Nullable String))
foreign import saveFileImpl
  :: forall r
   . { title :: String, filters :: Array Filter | r }
  -> Effect (Promise (Nullable String))
foreign import chooseImpl
  :: String
  -> { title :: String, buttons :: { yes :: String, no :: String, cancel :: String } }
  -> Effect (Promise String)
