//! Opt-in detector for the transient background-corruption artifact.
//!
//! Set `AJ_RENDER_DEBUG` to a file path and every frame is inspected right
//! after it is emitted. Frames that show the "speckle" signature append a
//! report to that file. Unset, the whole thing is one atomic load per frame
//! and nothing else.
//!
//! We look for three things and record which fired, so a report tells us
//! whether the corruption is in compositing or in the diff:
//!
//! - Composite speckle: the freshly composited front [`Screen`] has a row
//!   where one background color forms two or more runs with a default-bg hole
//!   between them. Full-width transcript boxes fill a row with one contiguous
//!   tint, so a hole means the composited surface itself is broken.
//! - Terminal speckle: the same scan over [`InternalScreen`] (our model of
//!   what the terminal now shows). A hole here that is not in the composite
//!   means the diff stranded stale cells.
//! - Divergence: cells whose background differs between the composite and the
//!   terminal model after a render. The two must agree once a render settles,
//!   so any divergence is a diff that failed to emit a needed change.
//!
//! When a row is flagged we also walk the [`Surface`] tree and list the
//! buffer-bearing subsurfaces covering that row, plus any sibling overlaps, to
//! attribute the bad cells to a specific surface.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cell::Color;
use crate::internal_screen::InternalScreen;
use crate::screen::Screen;
use crate::vxfw::Surface;

/// The report target, resolved once from `AJ_RENDER_DEBUG`.
fn target() -> Option<&'static PathBuf> {
    static TARGET: OnceLock<Option<PathBuf>> = OnceLock::new();
    TARGET
        .get_or_init(|| std::env::var_os("AJ_RENDER_DEBUG").map(PathBuf::from))
        .as_ref()
}

/// Whether the detector is armed. Cheap enough to call every frame.
pub(crate) fn enabled() -> bool {
    target().is_some()
}

/// Monotonic frame counter, only meaningful while the detector is armed.
static FRAME: AtomicU64 = AtomicU64::new(0);

/// A fragmented background color on one row: the color, its span, and the
/// default-bg columns that break it.
struct Speckle {
    row: usize,
    color: Color,
    first: usize,
    last: usize,
    holes: Vec<usize>,
}

/// Scans `bg` (one row of background colors) for a color whose occurrences
/// span a range with a default-bg hole inside it. Returns one entry per such
/// color.
fn scan_row(row: usize, bg: &[Color]) -> Vec<Speckle> {
    let mut colors: Vec<Color> = Vec::new();
    for c in bg {
        if *c != Color::Default && !colors.iter().any(|k| k.eql(c)) {
            colors.push(*c);
        }
    }
    let mut out = Vec::new();
    for color in colors {
        let first = bg.iter().position(|c| c.eql(&color));
        let last = bg.iter().rposition(|c| c.eql(&color));
        let (Some(first), Some(last)) = (first, last) else {
            continue;
        };
        let holes: Vec<usize> = (first..=last)
            .filter(|&i| bg[i] == Color::Default)
            .collect();
        if !holes.is_empty() {
            out.push(Speckle {
                row,
                color,
                first,
                last,
                holes,
            });
        }
    }
    out
}

/// A buffer-bearing surface located in absolute screen coordinates.
struct Rect {
    path: String,
    row0: i32,
    col0: i32,
    rows: i32,
    cols: i32,
}

impl Rect {
    fn covers_row(&self, row: usize) -> bool {
        let Ok(r) = i32::try_from(row) else {
            return false;
        };
        r >= self.row0 && r < self.row0 + self.rows
    }

    fn intersects(&self, other: &Rect) -> bool {
        self.col0 < other.col0 + other.cols
            && other.col0 < self.col0 + self.cols
            && self.row0 < other.row0 + other.rows
            && other.row0 < self.row0 + self.rows
    }
}

/// Walks the surface tree, appending a [`Rect`] for every subsurface that owns
/// a non-empty cell buffer. `path` names the position of each surface in the
/// tree so a report can point at one.
fn collect_rects(surface: &Surface, row0: i32, col0: i32, path: &str, out: &mut Vec<Rect>) {
    if !surface.buffer.is_empty() {
        out.push(Rect {
            path: path.to_string(),
            row0,
            col0,
            rows: i32::from(surface.size.height),
            cols: i32::from(surface.size.width),
        });
    }
    for (i, child) in surface.children.iter().enumerate() {
        let child_path = format!("{path}/{i}");
        collect_rects(
            &child.surface,
            row0 + child.origin.row,
            col0 + child.origin.col,
            &child_path,
            out,
        );
    }
}

/// Inspects a just-emitted frame. Appends a report to the target file when the
/// composite, the terminal model, or their divergence shows the artifact.
pub(crate) fn inspect_frame(surface: &Surface, front: &Screen, last: &InternalScreen) {
    let Some(path) = target() else {
        return;
    };
    let frame = FRAME.fetch_add(1, Ordering::Relaxed);

    let width = usize::from(front.width);
    let height = usize::from(front.height);
    if width == 0 || height == 0 || front.buf.len() != width * height {
        return;
    }
    // Geometry can differ for a frame around a resize. Skip rather than index
    // the shorter buffer out of range.
    if last.buf.len() != front.buf.len() {
        return;
    }

    let mut composite = Vec::new();
    let mut terminal = Vec::new();
    let mut diverge_rows: Vec<(usize, usize)> = Vec::new();
    // Cells the diff's default fast-path will mishandle: a non-default
    // background on a cell still flagged `default`. `eql` treats any two
    // `default` cells as equal, so once the content moves, a stale tint here
    // is never cleared.
    let mut tainted: Vec<(usize, usize, Color)> = Vec::new();
    for row in 0..height {
        let base = row * width;
        let front_bg: Vec<Color> = (0..width).map(|c| front.buf[base + c].style.bg).collect();
        let last_bg: Vec<Color> = (0..width).map(|c| last.buf[base + c].style.bg).collect();
        composite.extend(scan_row(row, &front_bg));
        terminal.extend(scan_row(row, &last_bg));
        let diff = (0..width)
            .filter(|&c| !front_bg[c].eql(&last_bg[c]))
            .count();
        if diff > 0 {
            diverge_rows.push((row, diff));
        }
        for c in 0..width {
            let cell = &front.buf[base + c];
            if cell.default && cell.style.bg != Color::Default {
                tainted.push((row, c, cell.style.bg));
            }
        }
    }

    if composite.is_empty() && terminal.is_empty() && diverge_rows.is_empty() && tainted.is_empty()
    {
        return;
    }

    let mut rects = Vec::new();
    collect_rects(surface, 0, 0, "", &mut rects);

    let flagged: Vec<usize> = composite
        .iter()
        .chain(terminal.iter())
        .map(|s| s.row)
        .chain(diverge_rows.iter().map(|(r, _)| *r))
        .chain(tainted.iter().map(|(r, _, _)| *r))
        .collect();

    let mut report = String::new();
    let _ = writeln!(
        report,
        "\n=== render_debug frame {frame} ({width}x{height}) ===",
    );
    for s in &composite {
        let _ = writeln!(
            report,
            "COMPOSITE speckle row {} color {:?} span [{}..={}] holes {:?}",
            s.row, s.color, s.first, s.last, s.holes,
        );
    }
    for s in &terminal {
        let _ = writeln!(
            report,
            "TERMINAL  speckle row {} color {:?} span [{}..={}] holes {:?}",
            s.row, s.color, s.first, s.last, s.holes,
        );
    }
    // Group tainted cells by row into contiguous column spans for a compact
    // report: these are the root-cause cells, a grey fill still flagged
    // `default`.
    if !tainted.is_empty() {
        let _ = writeln!(
            report,
            "TAINTED   {} composite cells carry a non-default bg while flagged default",
            tainted.len(),
        );
        let mut by_row: std::collections::BTreeMap<usize, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (row, col, _) in &tainted {
            by_row.entry(*row).or_default().push(*col);
        }
        for (row, cols) in by_row.iter().take(8) {
            let _ = writeln!(
                report,
                "  row {row}: {} tainted cells, first {:?}",
                cols.len(),
                &cols[..cols.len().min(8)],
            );
        }
    }

    for (row, diff) in &diverge_rows {
        let _ = writeln!(
            report,
            "DIVERGE   row {row}: {diff} cells differ (composite vs terminal model)",
        );
        // Dump the first handful of diverging cells with the flags the diff
        // keys on, plus the eql verdict. A skipped clear shows up as
        // `eql=true` on cells whose bg differs, which only the default
        // fast-path produces.
        let base = row * width;
        let mut shown = 0;
        for c in 0..width {
            let f = &front.buf[base + c];
            let l = &last.buf[base + c];
            if f.style.bg.eql(&l.style.bg) {
                continue;
            }
            let _ = writeln!(
                report,
                "    col {c}: front(char {:?} bg {:?} default {}) last(char {:?} bg {:?} default {}) eql={}",
                f.char.grapheme(),
                f.style.bg,
                f.default,
                l.char.as_str(),
                l.style.bg,
                l.default,
                l.eql(f),
            );
            shown += 1;
            if shown >= 6 {
                break;
            }
        }
    }

    // Attribute each flagged row to the surfaces covering it, and flag sibling
    // overlaps that are the usual cause of a stray fill bleeding across a row.
    for row in dedup(flagged) {
        let covering: Vec<&Rect> = rects.iter().filter(|r| r.covers_row(row)).collect();
        let _ = writeln!(
            report,
            "  row {row} covered by {} surfaces:",
            covering.len()
        );
        for r in &covering {
            let _ = writeln!(
                report,
                "    {} at (row {}, col {}) size {}x{}",
                r.path, r.row0, r.col0, r.rows, r.cols,
            );
        }
        for (i, a) in covering.iter().enumerate() {
            for b in &covering[i + 1..] {
                // Skip ancestor/descendant pairs: a child always sits inside
                // its parent, so only sibling (disjoint-path) overlaps are
                // interesting.
                let nested = a.path.starts_with(&b.path) || b.path.starts_with(&a.path);
                if !nested && a.intersects(b) {
                    let _ = writeln!(report, "    OVERLAP {} and {}", a.path, b.path);
                }
            }
        }
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(report.as_bytes());
    }
}

fn dedup(mut rows: Vec<usize>) -> Vec<usize> {
    rows.sort_unstable();
    rows.dedup();
    rows
}
