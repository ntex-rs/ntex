//! Backtrace
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::panic::Location;
use std::{cell::RefCell, fmt, fmt::Write, os, path, ptr, sync::Arc};

use backtrace::{BacktraceFmt, BacktraceFrame, BytesOrWideString};

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
pub struct Backtrace(Arc<str>);

impl Backtrace {
    /// Create new backtrace
    pub fn new(loc: &Location<'_>) -> Self {
        let repr = FRAMES.with(|c| {
            let mut cache = c.borrow_mut();
            let mut idx = 0;
            let mut st = foldhash::fast::FixedState::default().build_hasher();
            let mut idxs: [*mut os::raw::c_void; 128] = [ptr::null_mut(); 128];

            backtrace::trace(|frm| {
                let ip = frm.ip();
                st.write_usize(ip as usize);
                cache.entry(ip).or_insert_with(|| {
                    let mut f = BacktraceFrame::from(frm.clone());
                    f.resolve();
                    f
                });
                idxs[idx] = ip;
                idx += 1;

                idx < 128
            });

            let id = st.finish();

            REPRS.with(|r| {
                let mut reprs = r.borrow_mut();
                if let Some(repr) = reprs.get(&id) {
                    repr.clone()
                } else {
                    let mut frames: [Option<&BacktraceFrame>; 128] = [None; 128];
                    for (idx, ip) in idxs.as_ref().iter().enumerate() {
                        if !ip.is_null() {
                            frames[idx] = Some(&cache[ip]);
                        }
                    }

                    find_loc(loc, &mut frames);

                    #[allow(static_mut_refs)]
                    {
                        if let Some(start) = unsafe { START } {
                            find_loc_start(start, &mut frames);
                        }
                        if let Some(start) = unsafe { START_ALT } {
                            find_loc_start(start, &mut frames);
                        }
                    }

                    let bt = Bt(&frames[..]);
                    let mut buf = String::new();
                    let _ = write!(&mut buf, "\n{bt:?}");
                    let repr: Arc<str> = Arc::from(buf);
                    reprs.insert(id, repr.clone());
                    repr
                }
            })
        });

        Self(repr)
    }

    /// Backtrace repr
    pub fn repr(&self) -> &str {
        &self.0
    }
}

fn find_loc(loc: &Location<'_>, frames: &mut [Option<&BacktraceFrame>]) {
    for (idx, frm) in frames.iter_mut().enumerate() {
        if let Some(f) = frm {
            for sym in f.symbols() {
                if let Some(fname) = sym.filename()
                    && fname.ends_with(loc.file())
                {
                    for f in frames.iter_mut().take(idx) {
                        *f = None;
                    }
                    return;
                }
            }
        } else {
            break;
        }
    }
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
            &["src", "wrk.rs"][..],
            &["src", "future.rs"][..],
            &["std", "src", "thread", "local.rs"][..],
        ] {
            let mut p = path::PathBuf::new();
            for s in item {
                p.push(s);
            }
            paths.push(format!("{:?}", p));
        }
        paths
    };
}

fn find_loc_start(loc: (&str, u32), frames: &mut [Option<&BacktraceFrame>]) {
    PATHS.with(|paths| {
        let mut idx = 0;
        'outter: while idx < frames.len() {
            if let Some(frm) = &frames[idx] {
                for sym in frm.symbols() {
                    if let Some(fname) = sym.filename() {
                        for p in paths {
                            if fname.ends_with(p) {
                                frames[idx] = None;
                                idx += 1;
                                continue 'outter;
                            }
                        }
                    }

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
    })
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
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Display for Backtrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
