//! Synchronous native primitives for a bundled PureScript JavaScript program.
//! The host has no graph, document, command-line or application lifecycle policy.

mod graphics;
mod process;
mod terminal;

use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use rquickjs::function::{Func, Rest};
use rquickjs::{
    Array, CaughtError, Context, Ctx, Exception, FromJs, Function, IntoJs, Object, Runtime, Value,
};

use graphics::Graphics;
use process::Process;
use terminal::Terminal;

pub struct FontAsset {
    pub family: &'static str,
    pub data: &'static [u8],
    pub weight: u16,
    pub italic: bool,
    /// Comma-separated HarfBuzz feature settings, e.g. `ss01=1,liga=0`.
    pub features: &'static str,
}

pub struct Config {
    /// Arguments excluding the executable name.
    pub arguments: Vec<String>,
    pub version: String,
    pub fonts: Vec<FontAsset>,
    pub application: PathBuf,
    pub ffmpeg: PathBuf,
}

type HostResult<T> = Result<T, Box<dyn Error>>;

fn failure(message: impl Into<String>) -> Box<dyn Error> {
    io::Error::other(message.into()).into()
}

struct Video {
    process: Process,
    pixels: Vec<u8>,
    width: i32,
    height: i32,
}

struct Host {
    config: Config,
    graphics: Option<Graphics>,
    videos: HashMap<u32, Video>,
    next_video: u32,
    terminal: Terminal,
    started: Instant,
    interrupted: Arc<AtomicBool>,
}

impl Host {
    fn graphics(&mut self) -> HostResult<&mut Graphics> {
        if self.graphics.is_none() {
            self.graphics = Some(Graphics::new(&self.config.fonts)?);
        }
        Ok(self.graphics.as_mut().expect("graphics initialized above"))
    }

    fn begin_video(
        &mut self,
        output: String,
        fps: i32,
        width: i32,
        height: i32,
    ) -> HostResult<u32> {
        if fps <= 0 || width <= 0 || height <= 0 {
            return Err(failure("video fps, width and height must be positive"));
        }
        let next = self
            .next_video
            .checked_add(1)
            .ok_or_else(|| failure("video handle space exhausted"))?;
        let byte_length = (width as usize)
            .checked_mul(height as usize)
            .and_then(|size| size.checked_mul(4))
            .ok_or_else(|| failure("video frame dimensions are too large"))?;
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(byte_length)?;
        pixels.resize(byte_length, 0);
        let arguments = vec![
            "-y".into(),
            "-loglevel".into(),
            "error".into(),
            "-nostdin".into(),
            "-f".into(),
            "rawvideo".into(),
            "-pixel_format".into(),
            "rgba".into(),
            "-video_size".into(),
            format!("{width}x{height}"),
            "-framerate".into(),
            fps.to_string(),
            "-i".into(),
            "pipe:0".into(),
            "-an".into(),
            "-c:v".into(),
            "libx264".into(),
            "-preset".into(),
            "medium".into(),
            "-crf".into(),
            "12".into(),
            "-tune".into(),
            "animation".into(),
            "-vf".into(),
            "pad=ceil(iw/2)*2:ceil(ih/2)*2".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-movflags".into(),
            "+faststart".into(),
            output,
        ];
        let process = Process::spawn(&self.config.ffmpeg, &arguments, self.interrupted.clone())?;
        let id = self.next_video;
        self.next_video = next;
        self.videos.insert(
            id,
            Video {
                process,
                pixels,
                width,
                height,
            },
        );
        Ok(id)
    }

    fn video_frame(&mut self, id: u32, surface: u32) -> HostResult<()> {
        // Remove while operating so any error drops/reaps the encoder at once.
        let mut video = self
            .videos
            .remove(&id)
            .ok_or_else(|| failure("video is not open"))?;
        self.graphics()?
            .read_rgba(surface, video.width, video.height, &mut video.pixels)?;
        video.pixels = video.process.write(std::mem::take(&mut video.pixels))?;
        self.videos.insert(id, video);
        Ok(())
    }

    fn finish_video(&mut self, id: u32) -> HostResult<()> {
        let video = self
            .videos
            .remove(&id)
            .ok_or_else(|| failure("video is not open"))?;
        let result = video.process.finish()?;
        if !result.success {
            return Err(failure(format!(
                "FFmpeg exited with status {}:\n{}",
                result.code, result.stderr
            )));
        }
        Ok(())
    }

    fn cancel_video(&mut self, id: u32) -> HostResult<()> {
        if let Some(video) = self.videos.remove(&id) {
            video.process.cancel()?;
        }
        Ok(())
    }

    fn application(
        &self,
        args: Vec<String>,
        source: Option<String>,
    ) -> HostResult<process::Outcome> {
        process::run_application(
            &self.config.application,
            &args,
            source,
            self.interrupted.clone(),
        )
    }

    fn sleep(&self, milliseconds: i32) -> HostResult<()> {
        if milliseconds < 0 {
            return Err(failure("sleep duration must not be negative"));
        }
        let deadline = Instant::now() + Duration::from_millis(milliseconds as u64);
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if self.interrupted.load(Ordering::Relaxed) {
                return Err(failure("interrupted"));
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
        Ok(())
    }

    fn shutdown(&mut self) -> HostResult<()> {
        let mut result = Ok(());
        for (_, video) in self.videos.drain() {
            let cancelled = video.process.cancel();
            if result.is_ok() {
                result = cancelled;
            }
        }
        let restored = self.terminal.restore();
        result.and(restored)
    }
}

fn native<'js, T>(ctx: &Ctx<'js>, result: HostResult<T>) -> rquickjs::Result<T> {
    result.map_err(|error| Exception::throw_message(ctx, &error.to_string()))
}

fn argument<'js, T: FromJs<'js>>(
    ctx: &Ctx<'js>,
    args: &[Value<'js>],
    index: usize,
) -> rquickjs::Result<T> {
    let value = args
        .get(index)
        .ok_or_else(|| Exception::throw_type(ctx, "missing native argument"))?;
    T::from_js(ctx, value.clone())
}

fn canvas_color(value: Object<'_>) -> rquickjs::Result<graphics::Color> {
    Ok([
        value.get("r")?,
        value.get("g")?,
        value.get("b")?,
        value.get("a")?,
    ])
}

fn canvas_affine(value: Object<'_>) -> rquickjs::Result<[f32; 6]> {
    Ok([
        value.get("a")?,
        value.get("b")?,
        value.get("c")?,
        value.get("d")?,
        value.get("e")?,
        value.get("f")?,
    ])
}

fn canvas_stroke(value: Object<'_>) -> rquickjs::Result<graphics::Stroke> {
    Ok(graphics::Stroke {
        color: canvas_color(value.get("color")?)?,
        width: value.get("width")?,
        join: value.get("join")?,
        cap: value.get("cap")?,
    })
}

fn canvas_path<'js>(ctx: &Ctx<'js>, value: Object<'js>) -> rquickjs::Result<skia_safe::Path> {
    let values: Array<'js> = value.get("values")?;
    let offset: i32 = value.get("offset")?;
    let length: i32 = value.get("length")?;
    let end = offset
        .checked_add(length)
        .filter(|&end| offset >= 0 && length >= 0 && (end as usize) <= values.len());
    let end = end.ok_or_else(|| Exception::throw_range(ctx, "invalid canvas path range"))?;
    native(
        ctx,
        graphics::path_from_values((offset as usize..end as usize).map(|index| {
            values
                .get::<f64>(index)
                .map_err(|error| failure(CaughtError::from_error(ctx, error).to_string()))
        })),
    )
}

fn dispatch<'js>(
    ctx: &Ctx<'js>,
    host: &mut Host,
    name: &str,
    args: &[Value<'js>],
) -> rquickjs::Result<Value<'js>> {
    macro_rules! arg {
        ($index:expr, $type:ty) => {
            argument::<$type>(ctx, args, $index)?
        };
    }
    macro_rules! result {
        ($value:expr) => {
            native(ctx, $value)?.into_js(ctx)
        };
    }
    match name {
        "arguments" => host.config.arguments.as_slice().into_js(ctx),
        "version" => host.config.version.as_str().into_js(ctx),
        "isStdinTTY" => io::stdin().is_terminal().into_js(ctx),
        "readStdin" => result!((|| -> HostResult<String> {
            let mut source = String::new();
            io::stdin().read_to_string(&mut source)?;
            Ok(source)
        })()),
        "readText" => result!(std::fs::read_to_string(arg!(0, String)).map_err(Into::into)),
        "writeText" => {
            result!(std::fs::write(arg!(0, String), arg!(1, String)).map_err(Into::into))
        }
        "stdout" => result!(write_output(&mut io::stdout().lock(), &arg!(0, String))),
        "stderr" => result!(write_output(&mut io::stderr().lock(), &arg!(0, String))),
        "absolutePath" => {
            let path = arg!(0, String);
            let path = native(ctx, absolute_path(&path))?;
            path.into_js(ctx)
        }
        "environment" => std::env::var(arg!(0, String))
            .unwrap_or_default()
            .into_js(ctx),
        "monotonicMilliseconds" => (host.started.elapsed().as_secs_f64() * 1000.0).into_js(ctx),
        "runApplication" => {
            let outcome = native(
                ctx,
                host.application(arg!(0, Vec<String>), arg!(1, Option<String>)),
            )?;
            let value = Object::new(ctx.clone())?;
            value.set("success", outcome.success)?;
            value.set("code", outcome.code)?;
            value.set("stderr", outcome.stderr)?;
            value.into_js(ctx)
        }
        "measureText" => {
            let font = arg!(0, String);
            let text = arg!(1, String);
            let measured = native(
                ctx,
                host.graphics()
                    .and_then(|graphics| graphics.measure(&font, &text)),
            )?;
            let value = Object::new(ctx.clone())?;
            value.set("width", measured.width)?;
            value.set("ascent", measured.ascent)?;
            value.set("descent", measured.descent)?;
            value.into_js(ctx)
        }
        "canvasCreate" => {
            let options = arg!(0, Object<'js>);
            let width = options.get("width")?;
            let height = options.get("height")?;
            result!(native(ctx, host.graphics())?.create(width, height))
        }
        "canvasRelease" => {
            result!(native(ctx, host.graphics())?.release(arg!(0, u32)))
        }
        "canvasReset" => {
            result!(native(ctx, host.graphics())?.reset(arg!(0, u32)))
        }
        "canvasClear" => {
            let id = arg!(0, u32);
            let color = canvas_color(arg!(1, Object<'js>))?;
            result!(native(ctx, host.graphics())?.clear(id, color))
        }
        "canvasSave" => {
            result!(native(ctx, host.graphics())?.save(arg!(0, u32)))
        }
        "canvasRestore" => {
            result!(native(ctx, host.graphics())?.restore(arg!(0, u32)))
        }
        "canvasSetTransform" => {
            let id = arg!(0, u32);
            let matrix = canvas_affine(arg!(1, Object<'js>))?;
            result!(native(ctx, host.graphics())?.set_transform(id, matrix))
        }
        "canvasTransform" => {
            let id = arg!(0, u32);
            let matrix = canvas_affine(arg!(1, Object<'js>))?;
            result!(native(ctx, host.graphics())?.transform(id, matrix))
        }
        "canvasClip" => {
            let id = arg!(0, u32);
            let path = canvas_path(ctx, arg!(1, Object<'js>))?;
            let even_odd = arg!(2, bool);
            result!(native(ctx, host.graphics())?.clip(id, &path, even_odd))
        }
        "canvasSetBlend" => {
            let id = arg!(0, u32);
            let mode = arg!(1, String);
            result!(native(ctx, host.graphics())?.set_blend(id, &mode))
        }
        "canvasSetBlur" => {
            let id = arg!(0, u32);
            let radius = arg!(1, f32);
            result!(native(ctx, host.graphics())?.set_blur(id, radius))
        }
        "canvasSetTint" => {
            let id = arg!(0, u32);
            let color = arg!(1, Option<Object<'js>>).map(canvas_color).transpose()?;
            result!(native(ctx, host.graphics())?.set_tint(id, color))
        }
        "canvasFillPath" => {
            let id = arg!(0, u32);
            let path = canvas_path(ctx, arg!(1, Object<'js>))?;
            let color = canvas_color(arg!(2, Object<'js>))?;
            result!(native(ctx, host.graphics())?.fill_path(id, &path, color))
        }
        "canvasStrokePath" => {
            let id = arg!(0, u32);
            let path = canvas_path(ctx, arg!(1, Object<'js>))?;
            let stroke = canvas_stroke(arg!(2, Object<'js>))?;
            result!(native(ctx, host.graphics())?.stroke_path(id, &path, stroke))
        }
        "canvasFillStrokePath" => {
            let id = arg!(0, u32);
            let path = canvas_path(ctx, arg!(1, Object<'js>))?;
            let color = canvas_color(arg!(2, Object<'js>))?;
            let stroke = canvas_stroke(arg!(3, Object<'js>))?;
            result!(native(ctx, host.graphics())?.fill_stroke_path(id, &path, color, stroke))
        }
        "canvasText" => {
            let id = arg!(0, u32);
            let options = arg!(1, Object<'js>);
            let font: String = options.get("font")?;
            let text: String = options.get("content")?;
            let x = options.get("x")?;
            let y = options.get("y")?;
            let color = canvas_color(options.get("color")?)?;
            let align = options.get("align")?;
            let baseline = options.get("baseline")?;
            result!(
                native(ctx, host.graphics())?.text(id, &font, &text, x, y, color, align, baseline)
            )
        }
        "canvasCirclePattern" => {
            let id = arg!(0, u32);
            let options = arg!(1, Object<'js>);
            let rect: Object<'js> = options.get("rect")?;
            let pattern = graphics::CirclePattern {
                rect: [
                    rect.get("x")?,
                    rect.get("y")?,
                    rect.get("width")?,
                    rect.get("height")?,
                ],
                background: canvas_color(options.get("background")?)?,
                ink: canvas_color(options.get("ink")?)?,
                tile: options.get("tile")?,
                radius: options.get("radius")?,
                origin: [options.get("originX")?, options.get("originY")?],
            };
            result!(native(ctx, host.graphics())?.circle_pattern(id, pattern))
        }
        "canvasSurface" => {
            let destination = arg!(0, u32);
            let source = arg!(1, u32);
            result!(native(ctx, host.graphics())?.surface(destination, source))
        }
        "canvasWritePng" => {
            let id = arg!(0, u32);
            let path = arg!(1, String);
            result!(native(ctx, host.graphics())?.png(id, &path))
        }
        "beginVideo" => {
            let options = arg!(0, Object<'js>);
            result!(host.begin_video(
                options.get("output")?,
                options.get("fps")?,
                options.get("width")?,
                options.get("height")?
            ))
        }
        "writeVideoFrame" => result!(host.video_frame(arg!(0, u32), arg!(1, u32))),
        "finishVideo" => result!(host.finish_video(arg!(0, u32))),
        "cancelVideo" => result!(host.cancel_video(arg!(0, u32))),
        "enterTerminal" => result!(host.terminal.enter()),
        "restoreTerminal" => result!(host.terminal.restore()),
        "terminalSize" => {
            let (width, height) = if io::stdin().is_terminal() || io::stdout().is_terminal() {
                native(ctx, crossterm::terminal::size().map_err(Into::into))?
            } else {
                (0, 0)
            };
            let value = Object::new(ctx.clone())?;
            value.set("width", width)?;
            value.set("height", height)?;
            value.into_js(ctx)
        }
        "readKey" => result!(host.terminal.key()),
        "sleepMilliseconds" => result!(host.sleep(arg!(0, i32))),
        _ => Err(Exception::throw_type(
            ctx,
            &format!("unknown native primitive: {name}"),
        )),
    }
}

fn write_output(output: &mut impl Write, text: &str) -> HostResult<()> {
    output.write_all(text.as_bytes())?;
    output.flush()?;
    Ok(())
}

fn absolute_path(path: &str) -> HostResult<String> {
    let path = Path::new(path);
    // Unlike canonicalize, absolute() works for an output that does not exist.
    let absolute = std::path::absolute(path)?;
    absolute
        .into_os_string()
        .into_string()
        .map_err(|_| failure("absolute path is not UTF-8"))
}

static RUN_LOCK: Mutex<()> = Mutex::new(());
static INTERRUPT: LazyLock<Result<Arc<AtomicBool>, String>> = LazyLock::new(|| {
    let flag = Arc::new(AtomicBool::new(false));
    let handler_flag = flag.clone();
    ctrlc::set_handler(move || {
        handler_flag.store(true, Ordering::Relaxed);
    })
    .map_err(|error| error.to_string())?;
    Ok(flag)
});

fn register<'js>(ctx: &Ctx<'js>, state: Rc<RefCell<Host>>) -> rquickjs::Result<()> {
    let host = Object::new(ctx.clone())?;
    host.set(
        "call",
        Func::from(
            move |ctx: Ctx<'js>,
                  name: String,
                  args: Rest<Value<'js>>|
                  -> rquickjs::Result<Value<'js>> {
                let mut state = state.try_borrow_mut().map_err(|_| {
                    Exception::throw_message(&ctx, "native host cannot be called recursively")
                })?;
                dispatch(&ctx, &mut state, &name, &args.0)
            },
        ),
    )?;
    ctx.globals().set("__purescriptNativeHost", host)
}

/// Execute a synchronous IIFE whose `globalThis.__purescriptMain()` returns an
/// integer exit status. Native exceptions become ordinary JS Error instances.
/// Terminal state and child processes are released on success, JS errors and
/// Rust unwinding. Only one terminal-owning runtime may run in a process at once.
pub fn run(script: &str, config: Config) -> Result<i32, Box<dyn Error>> {
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
        terminal: Terminal::default(),
        started: Instant::now(),
        interrupted: interrupted.clone(),
    }));
    let runtime = Runtime::new()?;
    let interrupt_flag = interrupted.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        interrupt_flag.load(Ordering::Relaxed)
    })));
    // Native stack allowance is independent of JS heap size; generated code can
    // recurse through real document trees without inheriting QuickJS's tiny default.
    runtime.set_max_stack_size(8 * 1024 * 1024);
    let context = Context::full(&runtime)?;
    let outcome = context.with(|ctx| -> HostResult<i32> {
        let execute = || -> rquickjs::Result<i32> {
            register(&ctx, state.clone())?;
            ctx.eval::<(), _>(script)?;
            let main: Function = ctx.globals().get("__purescriptMain")?;
            let status: f64 = main.call(())?;
            if !status.is_finite()
                || status.fract() != 0.0
                || status < i32::MIN as f64
                || status > i32::MAX as f64
            {
                return Err(Exception::throw_type(
                    &ctx,
                    "__purescriptMain must return an integer exit status",
                ));
            }
            Ok(status as i32)
        };
        execute().map_err(|error| failure(CaughtError::from_error(&ctx, error).to_string()))
    });
    let cleanup = state.borrow_mut().shutdown();
    // Drop the context before the runtime, never retain a JS value in native
    // state, and never let a callback outlive the engine that created it.
    drop(context);
    drop(runtime);
    cleanup?;
    if interrupted.load(Ordering::Relaxed) {
        Ok(130)
    } else {
        outcome
    }
}
