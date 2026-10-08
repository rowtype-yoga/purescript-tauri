//! macOS JavaScriptCore implementation of the synchronous native host.
//!
//! This uses Apple's supported C API directly. Keeping the bridge in C values
//! avoids serializing drawing calls and lets path values stay in the JavaScript
//! array until Skia consumes each numeric element.

use super::{absolute_path, failure, write_output, Config, Host, HostResult, INTERRUPT, RUN_LOCK};
use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::ffi::{c_char, c_void};
use std::io::{self, IsTerminal};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use super::graphics;

#[allow(non_camel_case_types)]
type JSContextRef = *const OpaqueJSContext;
#[allow(non_camel_case_types)]
type JSContextGroupRef = *const OpaqueJSContextGroup;
#[allow(non_camel_case_types)]
type JSGlobalContextRef = *mut OpaqueJSContext;
#[allow(non_camel_case_types)]
type JSStringRef = *mut OpaqueJSString;
#[allow(non_camel_case_types)]
type JSValueRef = *const OpaqueJSValue;
#[allow(non_camel_case_types)]
type JSObjectRef = *mut OpaqueJSValue;

#[repr(C)]
struct OpaqueJSContext {
    _private: [u8; 0],
}
#[repr(C)]
struct OpaqueJSContextGroup {
    _private: [u8; 0],
}
#[repr(C)]
struct OpaqueJSString {
    _private: [u8; 0],
}
#[repr(C)]
struct OpaqueJSValue {
    _private: [u8; 0],
}

type JSObjectCallAsFunctionCallback = unsafe extern "C" fn(
    JSContextRef,
    JSObjectRef,
    JSObjectRef,
    usize,
    *const JSValueRef,
    *mut JSValueRef,
) -> JSValueRef;
type JSShouldTerminateCallback = unsafe extern "C" fn(JSContextRef, *mut c_void) -> bool;

#[link(name = "JavaScriptCore", kind = "framework")]
unsafe extern "C" {
    fn JSContextGroupCreate() -> JSContextGroupRef;
    fn JSContextGroupRelease(group: JSContextGroupRef);
    // Declared by JavaScriptCore's C API watchdog header on macOS. The SDK's
    // umbrella header omits it, but the framework exports it since macOS 10.6.
    fn JSContextGroupSetExecutionTimeLimit(
        group: JSContextGroupRef,
        limit: f64,
        callback: Option<JSShouldTerminateCallback>,
        context: *mut c_void,
    );
    fn JSContextGroupClearExecutionTimeLimit(group: JSContextGroupRef);
    fn JSGlobalContextCreateInGroup(
        group: JSContextGroupRef,
        class: *const c_void,
    ) -> JSGlobalContextRef;
    fn JSGlobalContextRelease(context: JSGlobalContextRef);
    fn JSContextGetGroup(context: JSContextRef) -> JSContextGroupRef;
    fn JSContextGetGlobalObject(context: JSContextRef) -> JSObjectRef;
    fn JSEvaluateScript(
        context: JSContextRef,
        script: JSStringRef,
        this_object: JSObjectRef,
        source_url: JSStringRef,
        starting_line: i32,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    fn JSObjectMake(context: JSContextRef, class: *const c_void, data: *mut c_void) -> JSObjectRef;
    fn JSObjectMakeArray(
        context: JSContextRef,
        count: usize,
        values: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    fn JSObjectIsFunction(context: JSContextRef, object: JSObjectRef) -> bool;
    fn JSObjectSetPropertyAtIndex(
        context: JSContextRef,
        object: JSObjectRef,
        index: u32,
        value: JSValueRef,
        exception: *mut JSValueRef,
    );
    fn JSObjectMakeFunctionWithCallback(
        context: JSContextRef,
        name: JSStringRef,
        callback: JSObjectCallAsFunctionCallback,
    ) -> JSObjectRef;
    fn JSObjectGetProperty(
        context: JSContextRef,
        object: JSObjectRef,
        name: JSStringRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    fn JSObjectSetProperty(
        context: JSContextRef,
        object: JSObjectRef,
        name: JSStringRef,
        value: JSValueRef,
        attributes: u32,
        exception: *mut JSValueRef,
    );
    fn JSObjectGetPropertyAtIndex(
        context: JSContextRef,
        object: JSObjectRef,
        index: u32,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    fn JSObjectCallAsFunction(
        context: JSContextRef,
        object: JSObjectRef,
        this_object: JSObjectRef,
        argument_count: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    fn JSValueGetType(context: JSContextRef, value: JSValueRef) -> i32;
    fn JSValueIsUndefined(context: JSContextRef, value: JSValueRef) -> bool;
    fn JSValueIsNull(context: JSContextRef, value: JSValueRef) -> bool;
    fn JSValueIsArray(context: JSContextRef, value: JSValueRef) -> bool;
    fn JSValueMakeUndefined(context: JSContextRef) -> JSValueRef;
    fn JSValueMakeBoolean(context: JSContextRef, value: bool) -> JSValueRef;
    fn JSValueMakeNumber(context: JSContextRef, value: f64) -> JSValueRef;
    fn JSValueMakeString(context: JSContextRef, value: JSStringRef) -> JSValueRef;
    fn JSValueProtect(context: JSContextRef, value: JSValueRef);
    fn JSValueUnprotect(context: JSContextRef, value: JSValueRef);
    fn JSValueToBoolean(context: JSContextRef, value: JSValueRef) -> bool;
    fn JSValueToNumber(context: JSContextRef, value: JSValueRef, exception: *mut JSValueRef)
        -> f64;
    fn JSValueToStringCopy(
        context: JSContextRef,
        value: JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSStringRef;
    fn JSStringCreateWithCharacters(characters: *const u16, length: usize) -> JSStringRef;
    fn JSStringRelease(value: JSStringRef);
    fn JSStringGetMaximumUTF8CStringSize(value: JSStringRef) -> usize;
    fn JSStringGetUTF8CString(value: JSStringRef, buffer: *mut c_char, size: usize) -> usize;
}

const JS_TYPE_BOOLEAN: i32 = 2;
const JS_TYPE_NUMBER: i32 = 3;
const JS_TYPE_STRING: i32 = 4;
const JS_TYPE_OBJECT: i32 = 5;
const WATCHDOG_PERIOD: f64 = 0.01;

struct JsString(JSStringRef);

impl JsString {
    fn new(value: &str) -> Result<Self, JsFailure> {
        let characters: Vec<u16> = value.encode_utf16().collect();
        let string = unsafe { JSStringCreateWithCharacters(characters.as_ptr(), characters.len()) };
        if string.is_null() {
            Err(JsFailure::error("could not allocate JavaScript string"))
        } else {
            Ok(Self(string))
        }
    }

    fn copy_to_string(value: JSStringRef) -> String {
        unsafe {
            let size = JSStringGetMaximumUTF8CStringSize(value);
            let mut bytes = Vec::<u8>::with_capacity(size);
            let written = JSStringGetUTF8CString(value, bytes.as_mut_ptr().cast(), size);
            if written == 0 {
                return String::new();
            }
            bytes.set_len(written - 1);
            match String::from_utf8(bytes) {
                Ok(value) => value,
                Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
            }
        }
    }
}

impl Drop for JsString {
    fn drop(&mut self) {
        unsafe { JSStringRelease(self.0) }
    }
}

struct ProtectedValue {
    context: JSContextRef,
    value: JSValueRef,
}

impl ProtectedValue {
    fn new(context: JSContextRef, value: JSValueRef) -> Self {
        unsafe { JSValueProtect(context, value) };
        Self { context, value }
    }
}

impl Drop for ProtectedValue {
    fn drop(&mut self) {
        unsafe { JSValueUnprotect(self.context, self.value) };
    }
}

enum JsFailure {
    Thrown(JSValueRef),
    Error(&'static str),
    Message(String),
    Type(&'static str),
    TypeMessage(String),
    Range(&'static str),
}

impl JsFailure {
    fn error(message: &'static str) -> Self {
        Self::Error(message)
    }

    fn native(error: Box<dyn Error>) -> Self {
        Self::Message(error.to_string())
    }
}

struct Api {
    context: JSContextRef,
}

impl Api {
    fn native<T>(&self, result: HostResult<T>) -> Result<T, JsFailure> {
        result.map_err(JsFailure::native)
    }

    fn check_exception(&self, value: JSValueRef) -> Result<(), JsFailure> {
        if value.is_null() {
            Ok(())
        } else {
            Err(JsFailure::Thrown(value))
        }
    }

    fn value_type(&self, value: JSValueRef) -> i32 {
        unsafe { JSValueGetType(self.context, value) }
    }

    fn argument<'a>(&self, args: &'a [JSValueRef], index: usize) -> Result<JSValueRef, JsFailure> {
        args.get(index)
            .copied()
            .ok_or(JsFailure::Type("missing native argument"))
    }

    fn string_value(&self, value: JSValueRef) -> Result<String, JsFailure> {
        if self.value_type(value) != JS_TYPE_STRING {
            return Err(JsFailure::Type("native argument must be a string"));
        }
        unsafe {
            let mut exception = ptr::null();
            let string_ref = JSValueToStringCopy(self.context, value, &mut exception);
            self.check_exception(exception)?;
            if string_ref.is_null() {
                return Err(JsFailure::error("could not read JavaScript string"));
            }
            let string = JsString::copy_to_string(string_ref);
            JSStringRelease(string_ref);
            Ok(string)
        }
    }

    fn arg_string(&self, args: &[JSValueRef], index: usize) -> Result<String, JsFailure> {
        self.string_value(self.argument(args, index)?)
    }

    fn optional_string(
        &self,
        args: &[JSValueRef],
        index: usize,
    ) -> Result<Option<String>, JsFailure> {
        let value = self.argument(args, index)?;
        if unsafe { JSValueIsUndefined(self.context, value) || JSValueIsNull(self.context, value) }
        {
            Ok(None)
        } else {
            self.string_value(value).map(Some)
        }
    }

    fn boolean_value(&self, value: JSValueRef) -> Result<bool, JsFailure> {
        if self.value_type(value) != JS_TYPE_BOOLEAN {
            return Err(JsFailure::Type("native argument must be a boolean"));
        }
        Ok(unsafe { JSValueToBoolean(self.context, value) })
    }

    fn boolean(&self, args: &[JSValueRef], index: usize) -> Result<bool, JsFailure> {
        self.boolean_value(self.argument(args, index)?)
    }

    fn number_value(&self, value: JSValueRef) -> Result<f64, JsFailure> {
        if self.value_type(value) != JS_TYPE_NUMBER {
            return Err(JsFailure::Type("native argument must be a number"));
        }
        unsafe {
            let mut exception = ptr::null();
            let number = JSValueToNumber(self.context, value, &mut exception);
            self.check_exception(exception)?;
            Ok(number)
        }
    }

    fn f32(&self, args: &[JSValueRef], index: usize) -> Result<f32, JsFailure> {
        Ok(self.number_value(self.argument(args, index)?)? as f32)
    }

    fn i32(&self, args: &[JSValueRef], index: usize) -> Result<i32, JsFailure> {
        let number = self.number_value(self.argument(args, index)?)?;
        if number < i32::MIN as f64 || number > i32::MAX as f64 {
            return Err(JsFailure::Range("native integer argument is out of range"));
        }
        Ok(number as i32)
    }

    fn u32(&self, args: &[JSValueRef], index: usize) -> Result<u32, JsFailure> {
        let number = self.number_value(self.argument(args, index)?)?;
        if number < 0.0 || number > u32::MAX as f64 {
            return Err(JsFailure::Range(
                "native unsigned integer argument is out of range",
            ));
        }
        Ok(number as u32)
    }

    fn object_value(&self, value: JSValueRef) -> Result<JSObjectRef, JsFailure> {
        if self.value_type(value) != JS_TYPE_OBJECT {
            return Err(JsFailure::Type("native argument must be an object"));
        }
        Ok(value.cast_mut())
    }

    fn object(&self, args: &[JSValueRef], index: usize) -> Result<JSObjectRef, JsFailure> {
        self.object_value(self.argument(args, index)?)
    }

    fn optional_object(
        &self,
        args: &[JSValueRef],
        index: usize,
    ) -> Result<Option<JSObjectRef>, JsFailure> {
        let value = self.argument(args, index)?;
        if unsafe { JSValueIsUndefined(self.context, value) || JSValueIsNull(self.context, value) }
        {
            Ok(None)
        } else {
            self.object_value(value).map(Some)
        }
    }

    fn property(&self, object: JSObjectRef, name: &str) -> Result<JSValueRef, JsFailure> {
        let name = JsString::new(name)?;
        unsafe {
            let mut exception = ptr::null();
            let value = JSObjectGetProperty(self.context, object, name.0, &mut exception);
            self.check_exception(exception)?;
            Ok(value)
        }
    }

    fn property_string(&self, object: JSObjectRef, name: &str) -> Result<String, JsFailure> {
        self.string_value(self.property(object, name)?)
    }

    fn property_f32(&self, object: JSObjectRef, name: &str) -> Result<f32, JsFailure> {
        Ok(self.number_value(self.property(object, name)?)? as f32)
    }

    fn property_i32(&self, object: JSObjectRef, name: &str) -> Result<i32, JsFailure> {
        let value = self.number_value(self.property(object, name)?)?;
        if value < i32::MIN as f64 || value > i32::MAX as f64 {
            return Err(JsFailure::Range("native integer property is out of range"));
        }
        Ok(value as i32)
    }

    fn property_u32(&self, object: JSObjectRef, name: &str) -> Result<u32, JsFailure> {
        let value = self.number_value(self.property(object, name)?)?;
        if value < 0.0 || value > u32::MAX as f64 {
            return Err(JsFailure::Range(
                "native unsigned integer property is out of range",
            ));
        }
        Ok(value as u32)
    }

    fn strings(&self, args: &[JSValueRef], index: usize) -> Result<Vec<String>, JsFailure> {
        let value = self.argument(args, index)?;
        if !unsafe { JSValueIsArray(self.context, value) } {
            return Err(JsFailure::Type("native argument must be an array"));
        }
        let array = value.cast_mut();
        let length = self.property_u32(array, "length")?;
        let mut values = Vec::with_capacity(length as usize);
        for index in 0..length {
            unsafe {
                let mut exception = ptr::null();
                let value = JSObjectGetPropertyAtIndex(self.context, array, index, &mut exception);
                self.check_exception(exception)?;
                values.push(self.string_value(value)?);
            }
        }
        Ok(values)
    }

    fn color(&self, object: JSObjectRef) -> Result<graphics::Color, JsFailure> {
        Ok([
            self.property_u32(object, "r")? as u8,
            self.property_u32(object, "g")? as u8,
            self.property_u32(object, "b")? as u8,
            self.property_u32(object, "a")? as u8,
        ])
    }

    fn affine(&self, object: JSObjectRef) -> Result<[f32; 6], JsFailure> {
        Ok([
            self.property_f32(object, "a")?,
            self.property_f32(object, "b")?,
            self.property_f32(object, "c")?,
            self.property_f32(object, "d")?,
            self.property_f32(object, "e")?,
            self.property_f32(object, "f")?,
        ])
    }

    fn stroke(&self, object: JSObjectRef) -> Result<graphics::Stroke, JsFailure> {
        Ok(graphics::Stroke {
            color: self.color(self.object_value(self.property(object, "color")?)?)?,
            width: self.property_f32(object, "width")?,
            join: self.property_i32(object, "join")?,
            cap: self.property_i32(object, "cap")?,
        })
    }

    fn path(&self, object: JSObjectRef) -> Result<skia_safe::Path, JsFailure> {
        let values_value = self.property(object, "values")?;
        if !unsafe { JSValueIsArray(self.context, values_value) } {
            return Err(JsFailure::Type("canvas path values must be an array"));
        }
        let values = self.object_value(values_value)?;
        let offset = self.property_i32(object, "offset")?;
        let length = self.property_i32(object, "length")?;
        let array_length = self.property_u32(values, "length")?;
        let end = offset
            .checked_add(length)
            .filter(|end| offset >= 0 && length >= 0 && (*end as u32) <= array_length)
            .ok_or(JsFailure::Range("invalid canvas path range"))?;
        self.native(graphics::path_from_values((offset as u32..end as u32).map(
            |index| unsafe {
                let mut exception = ptr::null();
                let value = JSObjectGetPropertyAtIndex(self.context, values, index, &mut exception);
                self.check_exception(exception)
                    .and_then(|()| self.number_value(value))
                    .map_err(|error| failure(self.failure_message(error)))
            },
        )))
    }

    fn failure_message(&self, error: JsFailure) -> String {
        match error {
            JsFailure::Thrown(value) => self.exception_string(value),
            JsFailure::Error(message) | JsFailure::Type(message) | JsFailure::Range(message) => {
                message.into()
            }
            JsFailure::Message(message) | JsFailure::TypeMessage(message) => message,
        }
    }

    fn undefined(&self) -> JSValueRef {
        unsafe { JSValueMakeUndefined(self.context) }
    }
    fn bool(&self, value: bool) -> JSValueRef {
        unsafe { JSValueMakeBoolean(self.context, value) }
    }
    fn number(&self, value: impl Into<f64>) -> JSValueRef {
        unsafe { JSValueMakeNumber(self.context, value.into()) }
    }

    fn string(&self, value: &str) -> Result<JSValueRef, JsFailure> {
        let value = JsString::new(value)?;
        Ok(unsafe { JSValueMakeString(self.context, value.0) })
    }

    fn argument_array(&self, arguments: &[String]) -> Result<JSValueRef, JsFailure> {
        let mut exception = ptr::null();
        let array = unsafe { JSObjectMakeArray(self.context, 0, ptr::null(), &mut exception) };
        self.check_exception(exception)?;
        if array.is_null() {
            return Err(JsFailure::error(
                "could not allocate JavaScript argument array",
            ));
        }
        // Insert each value immediately into a rooted array rather than retain
        // unrooted JS strings in a Rust heap vector while subsequent strings allocate.
        let array_root = ProtectedValue::new(self.context, array.cast_const());
        for (index, value) in arguments.iter().enumerate() {
            let value = self.string(value)?;
            unsafe {
                JSObjectSetPropertyAtIndex(
                    self.context,
                    array,
                    index as u32,
                    value,
                    &mut exception,
                );
            }
            self.check_exception(exception)?;
        }
        Ok(array_root.value)
    }

    fn object_output(&self) -> JSObjectRef {
        unsafe { JSObjectMake(self.context, ptr::null(), ptr::null_mut()) }
    }

    fn set(&self, object: JSObjectRef, name: &str, value: JSValueRef) -> Result<(), JsFailure> {
        let name = JsString::new(name)?;
        unsafe {
            let mut exception = ptr::null();
            JSObjectSetProperty(self.context, object, name.0, value, 0, &mut exception);
            self.check_exception(exception)
        }
    }

    fn exception_string(&self, value: JSValueRef) -> String {
        let description = unsafe {
            let mut exception = ptr::null();
            let string = JSValueToStringCopy(self.context, value, &mut exception);
            if self.check_exception(exception).is_err() || string.is_null() {
                return "JavaScript exception (message could not be read)".into();
            }
            let result = JsString::copy_to_string(string);
            JSStringRelease(string);
            result
        };
        if self.value_type(value) == JS_TYPE_OBJECT {
            if let Ok(stack) = self.property(value.cast_mut(), "stack") {
                if let Ok(stack) = self.string_value(stack) {
                    if !stack.is_empty() && !stack.starts_with(&description) {
                        return format!("{description}\n{stack}");
                    }
                }
            }
        }
        description
    }

    fn make_exception(&self, error: JsFailure) -> JSValueRef {
        if let JsFailure::Thrown(value) = error {
            return value;
        }
        let (name, message) = match error {
            JsFailure::Error(message) => ("Error", message.into()),
            JsFailure::Message(message) => ("Error", message),
            JsFailure::Type(message) => ("TypeError", message.into()),
            JsFailure::TypeMessage(message) => ("TypeError", message),
            JsFailure::Range(message) => ("RangeError", message.into()),
            JsFailure::Thrown(_) => unreachable!(),
        };
        let script = format!("new {name}({})", js_string_literal(&message));
        let script = match JsString::new(&script) {
            Ok(value) => value,
            Err(_) => return self.undefined(),
        };
        unsafe {
            let mut exception = ptr::null();
            let value = JSEvaluateScript(
                self.context,
                script.0,
                ptr::null_mut(),
                ptr::null_mut(),
                1,
                &mut exception,
            );
            if exception.is_null() {
                value
            } else {
                exception
            }
        }
    }
}

fn js_string_literal(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\u{2028}' => quoted.push_str("\\u2028"),
            '\u{2029}' => quoted.push_str("\\u2029"),
            character if character.is_control() => {
                use std::fmt::Write;
                let _ = write!(quoted, "\\u{:04x}", character as u32);
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn dispatch(
    api: &Api,
    host: &mut Host,
    name: &str,
    args: &[JSValueRef],
) -> Result<JSValueRef, JsFailure> {
    macro_rules! result {
        ($value:expr) => {
            api.native($value).map(|_| api.undefined())
        };
    }
    match name {
        "arguments" => api.argument_array(&host.config.arguments),
        "version" => api.string(&host.config.version),
        "isStdinTTY" => Ok(api.bool(io::stdin().is_terminal())),
        "readStdin" => api
            .native(host.read_stdin())
            .and_then(|value| api.string(&value)),
        "readLine" => match api.native(host.read_line())? {
            Some(value) => api.string(&value),
            None => Ok(api.undefined()),
        },
        "readText" => api
            .native(std::fs::read_to_string(api.arg_string(args, 0)?).map_err(Into::into))
            .and_then(|value| api.string(&value)),
        "writeText" => result!(
            std::fs::write(api.arg_string(args, 0)?, api.arg_string(args, 1)?).map_err(Into::into)
        ),
        "stdout" => result!(write_output(
            &mut io::stdout().lock(),
            &api.arg_string(args, 0)?
        )),
        "stderr" => result!(write_output(
            &mut io::stderr().lock(),
            &api.arg_string(args, 0)?
        )),
        "absolutePath" => api
            .native(absolute_path(&api.arg_string(args, 0)?))
            .and_then(|value| api.string(&value)),
        "environment" => api.string(&std::env::var(api.arg_string(args, 0)?).unwrap_or_default()),
        "monotonicMilliseconds" => Ok(api.number(host.started.elapsed().as_secs_f64() * 1000.0)),
        "platform" => api.string(std::env::consts::OS),
        "runApplicationTask" | "runApplication" => {
            let outcome = if name == "runApplicationTask" {
                api.native(
                    host.application_task(api.strings(args, 0)?, api.optional_string(args, 1)?),
                )?
            } else {
                api.native(host.application(api.strings(args, 0)?, api.optional_string(args, 1)?))?
            };
            let value = api.object_output();
            api.set(value, "success", api.bool(outcome.success))?;
            api.set(value, "code", api.number(outcome.code))?;
            api.set(value, "stderr", api.string(&outcome.stderr)?)?;
            Ok(value.cast_const())
        }
        "measureText" => {
            let font = api.arg_string(args, 0)?;
            let text = api.arg_string(args, 1)?;
            let measured = api.native(
                host.graphics()
                    .and_then(|graphics| graphics.measure(&font, &text)),
            )?;
            let value = api.object_output();
            api.set(value, "width", api.number(measured.width))?;
            api.set(value, "ascent", api.number(measured.ascent))?;
            api.set(value, "descent", api.number(measured.descent))?;
            Ok(value.cast_const())
        }
        "canvasCreate" => {
            let backend = api.arg_string(args, 0)?;
            let options = api.object(args, 1)?;
            let graphics = api.native(host.graphics())?;
            api.native(graphics.create(
                &backend,
                api.property_i32(options, "width")?,
                api.property_i32(options, "height")?,
            ))
            .map(|value| api.number(value))
        }
        "canvasRelease" => result!(api.native(host.graphics())?.release(api.u32(args, 0)?)),
        "canvasReset" => result!(api.native(host.graphics())?.reset(api.u32(args, 0)?)),
        "canvasClear" => {
            let id = api.u32(args, 0)?;
            let color = api.color(api.object(args, 1)?)?;
            result!(api.native(host.graphics())?.clear(id, color))
        }
        "canvasSave" => result!(api.native(host.graphics())?.save(api.u32(args, 0)?)),
        "canvasRestore" => result!(api.native(host.graphics())?.restore(api.u32(args, 0)?)),
        "canvasSetTransform" => {
            let id = api.u32(args, 0)?;
            let matrix = api.affine(api.object(args, 1)?)?;
            result!(api.native(host.graphics())?.set_transform(id, matrix))
        }
        "canvasTransform" => {
            let id = api.u32(args, 0)?;
            let matrix = api.affine(api.object(args, 1)?)?;
            result!(api.native(host.graphics())?.transform(id, matrix))
        }
        "canvasClip" => {
            let id = api.u32(args, 0)?;
            let path = api.path(api.object(args, 1)?)?;
            let even_odd = api.boolean(args, 2)?;
            result!(api.native(host.graphics())?.clip(id, &path, even_odd))
        }
        "canvasSetBlend" => {
            let id = api.u32(args, 0)?;
            let mode = api.arg_string(args, 1)?;
            result!(api.native(host.graphics())?.set_blend(id, &mode))
        }
        "canvasSetBlur" => {
            let id = api.u32(args, 0)?;
            let radius = api.f32(args, 1)?;
            result!(api.native(host.graphics())?.set_blur(id, radius))
        }
        "canvasSetTint" => {
            let id = api.u32(args, 0)?;
            let color = api
                .optional_object(args, 1)?
                .map(|value| api.color(value))
                .transpose()?;
            result!(api.native(host.graphics())?.set_tint(id, color))
        }
        "canvasFillPath" => {
            let id = api.u32(args, 0)?;
            let path = api.path(api.object(args, 1)?)?;
            let color = api.color(api.object(args, 2)?)?;
            result!(api.native(host.graphics())?.fill_path(id, &path, color))
        }
        "canvasStrokePath" => {
            let id = api.u32(args, 0)?;
            let path = api.path(api.object(args, 1)?)?;
            let stroke = api.stroke(api.object(args, 2)?)?;
            result!(api.native(host.graphics())?.stroke_path(id, &path, stroke))
        }
        "canvasFillStrokePath" => {
            let id = api.u32(args, 0)?;
            let path = api.path(api.object(args, 1)?)?;
            let color = api.color(api.object(args, 2)?)?;
            let stroke = api.stroke(api.object(args, 3)?)?;
            result!(api
                .native(host.graphics())?
                .fill_stroke_path(id, &path, color, stroke))
        }
        "canvasText" => {
            let id = api.u32(args, 0)?;
            let options = api.object(args, 1)?;
            let font = api.property_string(options, "font")?;
            let text = api.property_string(options, "content")?;
            let x = api.property_f32(options, "x")?;
            let y = api.property_f32(options, "y")?;
            let color = api.color(api.object_value(api.property(options, "color")?)?)?;
            let align = api.property_i32(options, "align")?;
            let baseline = api.property_i32(options, "baseline")?;
            result!(api
                .native(host.graphics())?
                .text(id, &font, &text, x, y, color, align, baseline))
        }
        "canvasCirclePattern" => {
            let id = api.u32(args, 0)?;
            let options = api.object(args, 1)?;
            let rect = api.object_value(api.property(options, "rect")?)?;
            let pattern = graphics::CirclePattern {
                rect: [
                    api.property_f32(rect, "x")?,
                    api.property_f32(rect, "y")?,
                    api.property_f32(rect, "width")?,
                    api.property_f32(rect, "height")?,
                ],
                background: api.color(api.object_value(api.property(options, "background")?)?)?,
                ink: api.color(api.object_value(api.property(options, "ink")?)?)?,
                tile: api.property_f32(options, "tile")?,
                radius: api.property_f32(options, "radius")?,
                origin: [
                    api.property_f32(options, "originX")?,
                    api.property_f32(options, "originY")?,
                ],
            };
            result!(api.native(host.graphics())?.circle_pattern(id, pattern))
        }
        "canvasSurface" => result!(api
            .native(host.graphics())?
            .surface(api.u32(args, 0)?, api.u32(args, 1)?)),
        "canvasWritePng" => {
            let id = api.u32(args, 0)?;
            let path = api.arg_string(args, 1)?;
            result!(api.native(host.graphics())?.png(id, &path))
        }
        "sharedFrameOpen" => result!(host.open_shared_frames()),
        "sharedFrameCapture" => api
            .native(host.capture_shared_frame(api.u32(args, 0)?))
            .map(|value| api.number(value)),
        "sharedFrameInfo" => {
            let id = api.u32(args, 0)?;
            let (name, width, height, byte_length) = api.native(host.shared_frame_info(id))?;
            let value = api.object_output();
            api.set(value, "id", api.number(id))?;
            api.set(
                value,
                "name",
                api.string(std::str::from_utf8(name).expect("shared-memory names are ASCII"))?,
            )?;
            api.set(value, "width", api.number(width))?;
            api.set(value, "height", api.number(height))?;
            api.set(value, "byteLength", api.number(byte_length))?;
            Ok(value.cast_const())
        }
        "sharedFrameRelease" => result!(host.release_shared_frame(api.u32(args, 0)?)),
        "sharedFrameClose" => result!(host.close_shared_frames()),
        "beginVideo" => {
            let options = api.object(args, 0)?;
            api.native(host.begin_video(
                api.property_string(options, "output")?,
                api.property_i32(options, "fps")?,
                api.property_i32(options, "width")?,
                api.property_i32(options, "height")?,
            ))
            .map(|value| api.number(value))
        }
        "writeVideoFrame" => result!(host.video_frame(api.u32(args, 0)?, api.u32(args, 1)?)),
        "finishVideo" => result!(host.finish_video(api.u32(args, 0)?)),
        "cancelVideo" => result!(host.cancel_video(api.u32(args, 0)?)),
        "enterTerminal" => result!(host.terminal.enter()),
        "restoreTerminal" => result!(host.terminal.restore()),
        "terminalSize" => {
            let (width, height) = if io::stdin().is_terminal() || io::stdout().is_terminal() {
                api.native(crossterm::terminal::size().map_err(Into::into))?
            } else {
                (0, 0)
            };
            let value = api.object_output();
            api.set(value, "width", api.number(width))?;
            api.set(value, "height", api.number(height))?;
            Ok(value.cast_const())
        }
        "terminalGraphicsOpen" => result!(host.open_terminal_graphics()),
        "terminalGraphicsSize" => {
            let (columns, rows, width, height) = api.native(host.terminal.graphics_size())?;
            let value = api.object_output();
            api.set(value, "columns", api.number(columns))?;
            api.set(value, "rows", api.number(rows))?;
            api.set(value, "width", api.number(width))?;
            api.set(value, "height", api.number(height))?;
            Ok(value.cast_const())
        }
        "terminalGraphicsPresent" => {
            result!(host.terminal_present(api.u32(args, 0)?, api.i32(args, 1)?, api.i32(args, 2)?))
        }
        "terminalGraphicsReadKey" | "readKey" => api
            .native(host.terminal.key())
            .and_then(|value| api.string(&value)),
        "terminalGraphicsClose" => result!(host.close_terminal_graphics()),
        "sleepMilliseconds" => result!(host.sleep(api.i32(args, 0)?)),
        _ => Err(JsFailure::TypeMessage(format!(
            "unknown native primitive: {name}"
        ))),
    }
}

struct JscHost {
    state: Rc<RefCell<Host>>,
}
thread_local! { static HOSTS: RefCell<HashMap<usize, Rc<JscHost>>> = RefCell::new(HashMap::new()); }

unsafe extern "C" fn native_call(
    context: JSContextRef,
    _function: JSObjectRef,
    _this: JSObjectRef,
    count: usize,
    values: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    let api = Api { context };
    let result = catch_unwind(AssertUnwindSafe(|| {
        let host = HOSTS
            .with(|hosts| hosts.borrow().get(&(context as usize)).cloned())
            .ok_or(JsFailure::error("native host is unavailable"))?;
        if count == 0 {
            return Err(JsFailure::Type("missing native primitive name"));
        }
        let values = unsafe { std::slice::from_raw_parts(values, count) };
        let name = api.string_value(values[0])?;
        let mut host = host
            .state
            .try_borrow_mut()
            .map_err(|_| JsFailure::error("native host cannot be called recursively"))?;
        dispatch(&api, &mut host, &name, &values[1..])
    }));
    match result {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            unsafe {
                *exception = api.make_exception(error);
            }
            api.undefined()
        }
        Err(_) => {
            unsafe {
                *exception = api.make_exception(JsFailure::error("native host panicked"));
            }
            api.undefined()
        }
    }
}

unsafe extern "C" fn should_terminate(context: JSContextRef, data: *mut c_void) -> bool {
    let interrupted = unsafe { (&*(data as *const AtomicBool)).load(Ordering::Relaxed) };
    if !interrupted {
        // Keep polling after a non-terminating callback, including on WebKit
        // versions that do not automatically restart the elapsed timer.
        unsafe {
            JSContextGroupSetExecutionTimeLimit(
                JSContextGetGroup(context),
                WATCHDOG_PERIOD,
                Some(should_terminate),
                data,
            );
        }
    }
    interrupted
}

struct Context {
    group: JSContextGroupRef,
    context: JSGlobalContextRef,
    state: Rc<RefCell<Host>>,
}

impl Context {
    fn new(state: Rc<RefCell<Host>>) -> HostResult<Self> {
        let group = unsafe { JSContextGroupCreate() };
        if group.is_null() {
            return Err(failure("could not create JavaScriptCore context group"));
        }
        let context = unsafe { JSGlobalContextCreateInGroup(group, ptr::null()) };
        if context.is_null() {
            unsafe { JSContextGroupRelease(group) };
            return Err(failure("could not create JavaScriptCore context"));
        }
        unsafe {
            JSContextGroupSetExecutionTimeLimit(
                group,
                WATCHDOG_PERIOD,
                Some(should_terminate),
                Arc::as_ptr(&state.borrow().interrupted).cast_mut().cast(),
            );
        }
        HOSTS.with(|hosts| {
            hosts.borrow_mut().insert(
                context as usize,
                Rc::new(JscHost {
                    state: state.clone(),
                }),
            );
        });
        Ok(Self {
            group,
            context,
            state,
        })
    }

    fn close(&mut self) -> HostResult<()> {
        if self.context.is_null() {
            return Ok(());
        }
        HOSTS.with(|hosts| {
            hosts.borrow_mut().remove(&(self.context as usize));
        });
        unsafe { JSContextGroupClearExecutionTimeLimit(self.group) };
        let cleanup = match self.state.try_borrow_mut() {
            Ok(mut state) => state.shutdown(),
            Err(_) => Err(failure("native host is still borrowed during shutdown")),
        };
        unsafe {
            JSGlobalContextRelease(self.context);
            JSContextGroupRelease(self.group);
        }
        self.context = ptr::null_mut();
        self.group = ptr::null();
        cleanup
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn execute(api: &Api, script: &str) -> Result<i32, JsFailure> {
    let global = unsafe { JSContextGetGlobalObject(api.context) };
    let call_name = JsString::new("call")?;
    let host = api.object_output();
    let call = unsafe { JSObjectMakeFunctionWithCallback(api.context, call_name.0, native_call) };
    api.set(host, "call", call.cast_const())?;
    api.set(global, "__purescriptNativeHost", host.cast_const())?;
    let source = JsString::new(script)?;
    let mut exception = ptr::null();
    unsafe {
        JSEvaluateScript(
            api.context,
            source.0,
            ptr::null_mut(),
            ptr::null_mut(),
            1,
            &mut exception,
        );
    }
    api.check_exception(exception)?;
    let main = api.property(global, "__purescriptMain")?;
    let main = api.object_value(main)?;
    if !unsafe { JSObjectIsFunction(api.context, main) } {
        return Err(JsFailure::Type("__purescriptMain must be a function"));
    }
    let status = unsafe {
        JSObjectCallAsFunction(
            api.context,
            main,
            ptr::null_mut(),
            0,
            ptr::null(),
            &mut exception,
        )
    };
    api.check_exception(exception)?;
    let status = api.number_value(status)?;
    if !status.is_finite()
        || status.fract() != 0.0
        || status < i32::MIN as f64
        || status > i32::MAX as f64
    {
        return Err(JsFailure::Type(
            "__purescriptMain must return an integer exit status",
        ));
    }
    Ok(status as i32)
}

pub(super) fn run(script: &str, config: Config) -> Result<i32, Box<dyn Error>> {
    let _guard = RUN_LOCK
        .lock()
        .map_err(|_| failure("native runtime lock is poisoned"))?;
    let interrupted = INTERRUPT
        .as_ref()
        .map_err(|error| failure(error.clone()))?
        .clone();
    interrupted.store(false, Ordering::Relaxed);
    let state = Rc::new(RefCell::new(Host {
        config,
        graphics: None,
        videos: HashMap::new(),
        next_video: 1,
        frames: HashMap::new(),
        next_frame: 1,
        shared_frames_active: false,
        terminal: Default::default(),
        stdin: io::BufReader::new(io::stdin()),
        started: Instant::now(),
        interrupted: interrupted.clone(),
    }));
    let mut context = Context::new(state)?;
    let api = Api {
        context: context.context.cast_const(),
    };
    let outcome = execute(&api, script).map_err(|error| failure(api.failure_message(error)));
    let cleanup = context.close();
    cleanup?;
    if interrupted.load(Ordering::Relaxed) {
        Ok(130)
    } else {
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::Config;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn config(arguments: Vec<String>) -> Config {
        Config {
            arguments,
            version: "test".into(),
            fonts: Vec::new(),
            application: PathBuf::new(),
            ffmpeg: PathBuf::new(),
        }
    }

    #[test]
    fn unicode_and_nul_survive_script_arguments_and_native_file_io() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tauri-jsc-🦀-{}-{}.txt",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let result = run(
            r#"
            globalThis.__purescriptMain = () => {
                const host = globalThis.__purescriptNativeHost;
                const path = host.call("arguments")[0];
                const text = "before\u0000🦀 after";
                host.call("writeText", path, text);
                return host.call("readText", path) === text ? 19 : 1;
            };
        "#,
            config(vec![path.to_string_lossy().into_owned()]),
        );
        let contents = std::fs::read_to_string(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(result.unwrap(), 19);
        assert_eq!(contents.unwrap(), "before\0🦀 after");
    }

    #[test]
    fn cancellation_remains_armed_after_earlier_watchdog_periods() {
        let path =
            std::env::temp_dir().join(format!("tauri-jsc-cancel-{}.txt", std::process::id()));
        let ready = path.clone();
        let stopper = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !ready.exists() {
                assert!(std::time::Instant::now() < deadline, "script never started");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            let signalled = std::time::Instant::now();
            crate::INTERRUPT
                .as_ref()
                .unwrap()
                .store(true, Ordering::Relaxed);
            signalled
        });
        let result = run(
            r#"
            globalThis.__purescriptMain = () => {
                const host = globalThis.__purescriptNativeHost;
                host.call("writeText", host.call("arguments")[0], "ready");
                const end = Date.now() + 2000;
                while (Date.now() < end) {}
                return 0;
            };
        "#,
            config(vec![path.to_string_lossy().into_owned()]),
        );
        let latency = stopper.join().unwrap().elapsed();
        let _ = std::fs::remove_file(path);
        assert_eq!(result.unwrap(), 130);
        assert!(
            latency < std::time::Duration::from_millis(500),
            "cancellation took {latency:?}"
        );
    }

    #[test]
    fn argument_arrays_keep_all_values_alive_during_allocation() {
        let arguments = (0..20_000)
            .map(|index| format!("{index}:{}\0🦀", "label".repeat(32)))
            .collect();
        let status = run(
            r#"
            globalThis.__purescriptMain = () => {
                const args = globalThis.__purescriptNativeHost.call("arguments");
                if (args.length !== 20000) return 1;
                for (let i = 0; i < args.length; ++i) {
                    if (args[i] !== i + ":" + "label".repeat(32) + "\u0000🦀") return 2;
                }
                return 23;
            };
        "#,
            config(arguments),
        )
        .unwrap();
        assert_eq!(status, 23);
    }

    #[test]
    fn invalid_entry_points_and_exit_statuses_do_not_poison_later_runs() {
        for body in [
            "42",
            "{}",
            "() => NaN",
            "() => Infinity",
            "() => 0.5",
            "() => 2147483648",
            "() => -2147483649",
        ] {
            let source = format!("globalThis.__purescriptMain = {body};");
            assert!(run(&source, config(Vec::new())).is_err(), "{body}");
        }
        assert_eq!(
            run("globalThis.__purescriptMain = () => 37", config(Vec::new())).unwrap(),
            37
        );
        assert_eq!(
            run(
                "globalThis.__purescriptMain = () => -2147483648",
                config(Vec::new())
            )
            .unwrap(),
            i32::MIN
        );
        assert_eq!(
            run(
                "globalThis.__purescriptMain = () => 2147483647",
                config(Vec::new())
            )
            .unwrap(),
            i32::MAX
        );
    }

    #[test]
    fn native_errors_keep_types_messages_and_a_usable_canvas() {
        let status = run(
            r#"
            globalThis.__purescriptMain = () => {
                const host = globalThis.__purescriptNativeHost;
                try { host.call("missingPrimitive"); return 1; }
                catch (error) { if (!(error instanceof TypeError)) return 2; }
                const canvas = host.call("canvasCreate", "cpu", {width:4,height:4});
                try {
                    host.call("canvasFillPath", canvas,
                        {values:[0,0,0],offset:-1,length:3}, {r:0,g:0,b:0,a:255});
                    return 3;
                } catch (error) { if (!(error instanceof RangeError)) return 4; }
                try { host.call("canvasClear", canvas, {r:"invalid",g:0,b:0,a:255}); return 5; }
                catch (error) { if (!(error instanceof TypeError)) return 6; }
                host.call("canvasClear", canvas, {r:12,g:34,b:56,a:255});
                host.call("canvasRelease", canvas);
                return 41;
            };
        "#,
            config(Vec::new()),
        )
        .unwrap();
        assert_eq!(status, 41);
        let error = run(
            r#"
            globalThis.__purescriptMain = () => { throw new Error("renderer cause 🦀"); };
        "#,
            config(Vec::new()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("renderer cause 🦀"), "{error}");
    }
}
