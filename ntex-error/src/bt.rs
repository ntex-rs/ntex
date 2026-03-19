//! Backtrace
#![allow(warnings)]
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::panic::Location;
use std::{cell::RefCell, fmt, fmt::Write, os, path, ptr, sync::Arc};

use backtrace::{BacktraceFmt, BacktraceFrame, BytesOrWideString, Frame};

thread_local! {
    static FRAMES: RefCell<HashMap<*mut os::raw::c_void, BacktraceFrame>> = RefCell::new(HashMap::default());
    static REPRS: RefCell<HashMap<u64, Arc<str>>> = RefCell::new(HashMap::default());
}
static mut START: Option<(&'static str, u32)> = None;
static mut START_ALT: Option<(&'static str, u32)> = None;

pub fn set_backtrace_start(file: &'static str, line: u32) {
    unsafe {
        START = Some((file, line));
    }
}

#[doc(hidden)]
pub fn set_backtrace_start_alt(file: &'static str, line: u32) {
    unsafe {
        START_ALT = Some((file, line));
    }
}

#[derive(Clone)]
/// Representation of a backtrace.
///
/// This structure can be used to capture a backtrace at various
/// points in a program and later used to inspect what the backtrace
/// was at that time.
pub struct Backtrace(Arc<BacktraceInner>);

struct BacktraceInner {
    id: u64,
    frames: [Option<Frame>; 80],
    location: &'static Location<'static>,
}

impl Backtrace {
    /// Create new backtrace
    pub fn new(location: &'static Location<'static>) -> Self {
        let mut st = foldhash::fast::FixedState::default().build_hasher();
        let mut idx = 0;
        let mut frames: [Option<Frame>; 80] = [const { None }; 80];

        backtrace::trace(|frm| {
            let ip = frm.ip();
            st.write_usize(ip as usize);
            frames[idx] = Some(frm.clone());
            idx += 1;
            idx < 80
        });
        let id = st.finish();

        Self(Arc::new(BacktraceInner {
            id,
            frames,
            location,
        }))
    }

    /// Backtrace repr
    pub fn repr(&self) -> Option<Arc<str>> {
        REPRS.with(|r| r.borrow_mut().get(&self.0.id).cloned())
    }

    pub fn is_resolved(&self) -> bool {
        REPRS.with(|r| r.borrow_mut().contains_key(&self.0.id))
    }

    pub fn resolve(&self) {
        REPRS.with(|r| {
            let mut reprs = r.borrow_mut();
            if reprs.contains_key(&self.0.id) {
                return;
            }

            FRAMES.with(|c| {
                let mut cache = c.borrow_mut();

                for frm in &self.0.frames {
                    if let Some(frm) = frm {
                        let ip = frm.ip();
                        cache.entry(ip).or_insert_with(|| {
                            let mut f = BacktraceFrame::from(frm.clone());
                            f.resolve();
                            f
                        });
                    }
                }

                let mut idx = 0;
                let mut frames: [Option<&BacktraceFrame>; 80] = [None; 80];
                for frm in &self.0.frames {
                    if let Some(frm) = frm {
                        frames[idx] = Some(&cache[&frm.ip()]);
                        idx += 1;
                    }
                }

                find_loc(self.0.location, &mut frames);

                #[allow(static_mut_refs)]
                {
                    if let Some(start) = unsafe { START } {
                        find_loc_start(start, &mut frames);
                    }
                    if let Some(start) = unsafe { START_ALT } {
                        find_loc_start(start, &mut frames);
                    }
                    PATHS2.with(|paths| {
                        for s in paths {
                            find_loc_start((s.as_str(), 0), &mut frames);
                        }
                    });
                }

                let mut idx = 0;
                for frm in &mut frames {
                    if frm.is_some() {
                        if idx > 10 {
                            *frm = None;
                        } else {
                            idx += 1;
                        }
                    }
                }

                let bt = Bt(&frames[..]);
                let mut buf = String::new();
                let _ = write!(&mut buf, "\n{bt:?}");
                let repr: Arc<str> = Arc::from(buf);
                reprs.insert(self.0.id, repr.clone());
                repr
            });
        });
    }
}

fn find_loc(loc: &Location<'_>, frames: &mut [Option<&BacktraceFrame>]) {
    let mut idx = 0;

    'outter: for (i, frm) in frames.iter().enumerate() {
        if let Some(f) = frm {
            for sym in f.symbols() {
                if let Some(fname) = sym.filename()
                    && fname.ends_with(loc.file())
                {
                    idx = i;
                    break 'outter;
                }
            }
        } else {
            break;
        }
    }

    for f in frames.iter_mut().take(idx) {
        *f = None;
    }

    PATHS.with(|paths| {
        'outter: for frm in &mut frames[idx..] {
            if let Some(f) = frm {
                for sym in f.symbols() {
                    if let Some(fname) = sym.filename() {
                        for p in paths {
                            if fname.ends_with(p) {
                                *frm = None;
                                continue 'outter;
                            }
                        }
                    }
                }
            }
        }
    });
}

thread_local! {
    static PATHS: Vec<String> = {
        let mut paths = Vec::new();
        for item in [
            &["src", "ctx.rs"][..],
            &["src", "map_err.rs"][..],
            &["src", "and_then.rs"][..],
            &["src", "fn_service.rs"][..],
            &["src", "pipeline.rs"][..],
            &["src", "net", "factory.rs"][..],
            &["src", "future", "future.rs"][..],
            &["src", "net", "service.rs"][..],
            &["src", "boxed.rs"][..],
            &["src", "error.rs"][..],
            &["src", "wrk.rs"][..],
            &["src", "future.rs"][..],
            &["std", "src", "thread", "local.rs"][..],
        ] {
            paths.push(item.iter().collect::<path::PathBuf>().to_string_lossy().into_owned());
        }
        paths
    };

    static PATHS2: Vec<String> = {
        let mut paths = Vec::new();
        for item in [
            &["src", "driver.rs"][..],
            &["src", "rt_compio.rs"][..],
            &["core", "src", "panic", "unwind_safe.rs"][..],
            &["src", "runtime", "task", "core.rs"][..]
        ] {
            paths.push(item.iter().collect::<path::PathBuf>().to_string_lossy().into_owned());
        }
        paths
    }
}

fn find_loc_start(loc: (&str, u32), frames: &mut [Option<&BacktraceFrame>]) {
    let mut idx = 0;
    while idx < frames.len() {
        if let Some(frm) = &frames[idx] {
            for sym in frm.symbols() {
                if let Some(fname) = sym.filename()
                    && let Some(lineno) = sym.lineno()
                    && fname.ends_with(loc.0)
                    && (loc.1 == 0 || lineno == loc.1)
                {
                    for f in frames.iter_mut().skip(idx) {
                        if f.is_some() {
                            *f = None;
                        }
                    }
                    return;
                }
            }
        }
        idx += 1;
    }
}

struct Bt<'a>(&'a [Option<&'a BacktraceFrame>]);

impl fmt::Debug for Bt<'_> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cwd = std::env::current_dir();
        let mut print_path =
            move |fmt: &mut fmt::Formatter<'_>, path: BytesOrWideString<'_>| {
                let path = path.into_path_buf();
                if let Ok(cwd) = &cwd
                    && let Ok(suffix) = path.strip_prefix(cwd)
                {
                    return fmt::Display::fmt(&suffix.display(), fmt);
                }
                fmt::Display::fmt(&path.display(), fmt)
            };

        let mut f = BacktraceFmt::new(fmt, backtrace::PrintFmt::Short, &mut print_path);
        f.add_context()?;
        for frm in self.0.iter().flatten() {
            f.frame().backtrace_frame(frm)?;
        }
        f.finish()?;
        Ok(())
    }
}

impl fmt::Debug for Backtrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(repr) = self.repr() {
            fmt::Display::fmt(repr.as_ref(), f)
        } else {
            Ok(())
        }
    }
}

impl fmt::Display for Backtrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(repr) = self.repr() {
            fmt::Display::fmt(repr.as_ref(), f)
        } else {
            Ok(())
        }
    }
}
