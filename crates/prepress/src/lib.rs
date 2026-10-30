//! pdfcraft-prepress — print production (L4). See the README.
//!
//! Printer marks are written as printer's mark annotations (ISO 32000-2 §12.5.6.20), one per
//! mark, each carrying the spec's `/MN` name and PdfCraft's [`PRODUCER_KEY`] so that
//! [`remove_printer_marks`] leaves another producer's marks alone.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod draw;
#[cfg(test)]
mod tests;

use pdfcraft_cos::{Dict, Document, ObjRef, Object, Stream};
use pdfcraft_model::Page;
use serde::Serialize;

/// `[x0, y0, x1, y1]` in PDF user space, ordered so `x0 <= x1` and `y0 <= y1`.
pub type Rect = [f64; 4];

/// Marks PdfCraft wrote carry this key, so removal never touches another producer's marks.
pub const PRODUCER_KEY: &[u8] = b"PCPrinterMark";
/// A page whose media box [`add_printer_marks`] enlarged keeps the original here, so that
/// [`remove_printer_marks`] can put it back.
pub const SAVED_MEDIA_KEY: &[u8] = b"PCMarksMediaBox";
/// Set beside [`SAVED_MEDIA_KEY`] when the page had no media box of its own and inherited one
/// (§7.7.3.4). Removal then takes the entry away again rather than leaving a direct box behind
/// where the file had none.
pub const SAVED_MEDIA_INHERITED_KEY: &[u8] = b"PCMarksMediaInherited";
/// A page whose crop box [`add_printer_marks`] enlarged keeps the original here. The crop box is
/// what a viewer shows and a RIP cuts to, so a mark outside it is invisible however big the media
/// box is; growing the page has to grow both.
pub const SAVED_CROP_KEY: &[u8] = b"PCMarksCropBox";
/// [`SAVED_MEDIA_INHERITED_KEY`] for the crop box: set when the page inherited one (§7.7.3.4)
/// rather than carrying its own.
pub const SAVED_CROP_INHERITED_KEY: &[u8] = b"PCMarksCropInherited";
/// The annotation flags every mark gets: Print (bit 3) and ReadOnly (bit 7), so marks go to the
/// press and cannot be dragged off the page by accident.
pub const MARK_FLAGS: i64 = 4 | 64;
/// The largest any style measurement may be, so a hostile or mistyped value cannot push a mark,
/// or a grown media box, out to a coordinate the writer would have to round away.
pub const MAX_MEASURE: f64 = 10_000.0;
/// Letter, the media box assumed when a page has none (what `pdfcraft-model` assumes too).
const DEFAULT_MEDIA: Rect = [0.0, 0.0, 612.0, 792.0];

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PrepressError {
    #[error("the document has no page tree")]
    NoPageTree,
    /// Holds the 0-based index; the message names the 1-based page a user counts (#1550).
    #[error("page {} does not exist", .0.saturating_add(1))]
    NoSuchPage(usize),
    #[error("{0}")]
    Invalid(String),
    /// Page-information text the standard fonts cannot draw; the message names the character.
    #[error("{0}")]
    Undrawable(String),
    #[error(transparent)]
    Cos(#[from] pdfcraft_cos::CosError),
}

type Result<T> = std::result::Result<T, PrepressError>;

fn invalid(s: &str) -> PrepressError {
    PrepressError::Invalid(s.to_owned())
}

/// Which page box the marks are placed around (ISO 32000-2 §14.11.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RefBox {
    Media,
    Crop,
    Bleed,
    /// Where the page is cut, and so what a crop mark marks. Acrobat's default too.
    #[default]
    Trim,
    Art,
}

impl RefBox {
    pub fn key(self) -> &'static [u8] {
        match self {
            RefBox::Media => b"MediaBox",
            RefBox::Crop => b"CropBox",
            RefBox::Bleed => b"BleedBox",
            RefBox::Trim => b"TrimBox",
            RefBox::Art => b"ArtBox",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().trim_end_matches("box") {
            "media" => Some(Self::Media),
            "crop" => Some(Self::Crop),
            "bleed" => Some(Self::Bleed),
            "trim" => Some(Self::Trim),
            "art" => Some(Self::Art),
            _ => None,
        }
    }
}

/// The size and placement of the marks, in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    /// Stroke width of the marks. Acrobat offers 0.125, 0.25 and 0.5.
    pub weight: f64,
    /// Length of one arm of a crop or bleed mark.
    pub length: f64,
    /// Gap between the box being marked and where its marks start, so that no mark prints
    /// inside the trimmed page.
    pub offset: f64,
    /// Radius of a registration target's circle; its crosshair reaches twice this.
    pub radius: f64,
    /// Height of the colour bar, and the side of one patch.
    pub patch: f64,
    /// Point size of the page-information text.
    pub font_size: f64,
}

impl Default for Style {
    fn default() -> Self {
        Self { weight: 0.25, length: 18.0, offset: 6.0, radius: 4.0, patch: 12.0, font_size: 7.0 }
    }
}

impl Style {
    /// The measurement that is not a usable length, if any, named for the message. A zero line
    /// weight is the thinnest the device can draw, and a zero offset puts the marks against the
    /// box, so both are allowed; the rest must be positive.
    fn unusable(&self) -> Option<&'static str> {
        let ok = |v: f64, zero: bool| v.is_finite() && v <= MAX_MEASURE && (v > 0.0 || (zero && v == 0.0));
        [
            (self.weight, true, "the line weight"),
            (self.length, false, "the mark length"),
            (self.offset, true, "the mark offset"),
            (self.radius, false, "the target radius"),
            (self.patch, false, "the patch size"),
            (self.font_size, false, "the text size"),
        ]
        .into_iter()
        .find_map(|(v, zero, what)| (!ok(v, zero)).then_some(what))
    }
}

/// The page-information line's text. The caller supplies both parts, so this crate never reads a
/// clock or a file path and the same input always draws the same page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Info {
    /// Drawn at the left: usually the document's file name.
    pub name: String,
    /// Drawn at the right: usually a date and time the caller formatted.
    pub stamp: String,
}

/// Which marks to add, how they look, and which box they go around.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Marks {
    /// Cut marks at the corners of the reference box.
    pub crop: bool,
    /// Cut marks at the corners of the bleed box.
    pub bleed: bool,
    /// Registration targets centred on each side.
    pub registration: bool,
    /// A `DeviceCMYK` colour bar below the reference box.
    pub color_bars: bool,
    /// A page-information line above the reference box.
    pub info: Option<Info>,
    pub style: Style,
    pub reference: RefBox,
    /// Enlarge each page's media box so that its marks fit inside it. Without this, the marks
    /// that fall outside are reported in [`Report::clipped`] and left where they are.
    pub grow_media: bool,
}

impl Marks {
    /// Whether any mark is selected at all.
    pub fn any(&self) -> bool {
        self.crop || self.bleed || self.registration || self.color_bars || self.info.is_some()
    }

    /// The first character of the page-information text the standard fonts cannot draw, which
    /// [`add_printer_marks`] refuses rather than writing as `?`.
    pub fn first_undrawable(&self) -> Option<char> {
        let info = self.info.as_ref()?;
        pdfcraft_fonts::first_undrawable(&info.name).or_else(|| pdfcraft_fonts::first_undrawable(&info.stamp))
    }
}

/// What adding the marks did. Page numbers are 1-based, as a user counts them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Mark annotations added.
    pub added: usize,
    /// Pages whose marks reach outside the media box or the crop box, neither of which was
    /// changed. The crop box counts because it is what a viewer shows and a RIP cuts to.
    pub clipped: Vec<usize>,
    /// Pages whose media box, and crop box where the page had one, were enlarged to fit the marks.
    pub grown: Vec<usize>,
    /// Pages where bleed marks were skipped because the bleed box matches the reference box, so
    /// they would have been drawn exactly under the crop marks.
    pub no_bleed: Vec<usize>,
}

/// A page's boxes, each resolved and clipped as ISO 32000-2 §14.11.2 requires.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Boxes {
    pub media: Rect,
    pub crop: Rect,
    pub bleed: Rect,
    pub trim: Rect,
    pub art: Rect,
}

impl Boxes {
    pub fn get(&self, which: RefBox) -> Rect {
        match which {
            RefBox::Media => self.media,
            RefBox::Crop => self.crop,
            RefBox::Bleed => self.bleed,
            RefBox::Trim => self.trim,
            RefBox::Art => self.art,
        }
    }
}

/// A finite, non-empty, ordered rectangle from a PDF array, or `None`.
fn rect(doc: &Document, o: Option<&Object>) -> Option<Rect> {
    let o = doc.resolve(o?);
    let v: Vec<f64> = o.as_array()?.iter().filter_map(|x| doc.resolve(x).as_f64()).collect();
    let [a, b, c, d] = <[f64; 4]>::try_from(v).ok()?;
    let r = [a.min(c), b.min(d), a.max(c), b.max(d)];
    (r.iter().all(|x| x.is_finite()) && r[2] > r[0] && r[3] > r[1]).then_some(r)
}

/// `r` clipped to `to`, or `to` when the two do not overlap in both directions.
fn clip(r: Rect, to: Rect) -> Rect {
    let c = [r[0].max(to[0]), r[1].max(to[1]), r[2].min(to[2]), r[3].min(to[3])];
    if c[2] > c[0] && c[3] > c[1] { c } else { to }
}

/// The smallest rectangle holding both.
fn union(a: Rect, b: Rect) -> Rect {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

/// Every box of `page`: the crop box defaults to the media box, and the bleed, trim and art
/// boxes to the crop box, each clipped to the media box.
pub fn boxes(doc: &Document, page: &Page) -> Boxes {
    let media = rect(doc, page.dict.get(b"MediaBox")).unwrap_or(DEFAULT_MEDIA);
    let crop = rect(doc, page.dict.get(b"CropBox")).map_or(media, |c| clip(c, media));
    let of = |key: &[u8]| rect(doc, page.dict.get(key)).map_or(crop, |r| clip(r, media));
    Boxes { media, crop, bleed: of(b"BleedBox"), trim: of(b"TrimBox"), art: of(b"ArtBox") }
}

/// Every leaf page, or an error when the document has no page tree.
fn pages(doc: &Document) -> Result<Vec<Page>> {
    let all = pdfcraft_model::pages(doc);
    if all.is_empty() { Err(PrepressError::NoPageTree) } else { Ok(all) }
}

/// Check a page selection against the document, as the other feature crates do.
fn check(selection: &[usize], total: usize) -> Result<()> {
    match selection.iter().copied().find(|p| *p >= total) {
        Some(p) => Err(PrepressError::NoSuchPage(p)),
        None => Ok(()),
    }
}

/// The marks one page gets, and whether bleed marks were asked for but skipped.
fn marks_for(page: usize, total: usize, b: &Boxes, m: &Marks) -> (Vec<draw::Mark>, bool) {
    let style = &m.style;
    let r = b.get(m.reference);
    let mut out = Vec::new();
    let corners = |rect: Rect, name: &'static str, out: &mut Vec<draw::Mark>| {
        for (x, y, sx, sy) in
            [(rect[0], rect[1], -1.0, -1.0), (rect[2], rect[1], 1.0, -1.0), (rect[0], rect[3], -1.0, 1.0), (rect[2], rect[3], 1.0, 1.0)]
        {
            out.push(draw::corner(name, [x, y], sx, sy, style));
        }
    };
    if m.crop {
        corners(r, "CropMark", &mut out);
    }
    // Bleed marks drawn on top of the crop marks would be invisible and would misreport the
    // bleed, so they are skipped and reported instead.
    let bleed_distinct = b.bleed != r;
    if m.bleed && bleed_distinct {
        corners(b.bleed, "BleedMark", &mut out);
    }
    if m.registration {
        let mid = [(r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0];
        let at = style.offset + draw::registration_reach(style);
        for centre in [[mid[0], r[1] - at], [mid[0], r[3] + at], [r[0] - at, mid[1]], [r[2] + at, mid[1]]] {
            out.push(draw::registration(centre, style));
        }
    }
    // The colour bar and the page information go beyond everything placed so far, measured from
    // what those marks actually occupy rather than from the style again. Deriving it twice is how
    // a bar ends up inside a registration target, or inside bleed marks that sit further out than
    // the reference box does.
    let below = out.iter().map(|k| r[1] - k.rect[1]).fold(0.0f64, f64::max);
    let above = out.iter().map(|k| k.rect[3] - r[3]).fold(0.0f64, f64::max);
    if m.color_bars {
        let x = (r[0] + r[2]) / 2.0 - draw::color_bar_width(style) / 2.0;
        out.push(draw::color_bar([x, r[1] - below - draw::BAND_GAP - style.patch], style));
    }
    if let Some(info) = &m.info {
        out.push(draw::page_info((r[0], r[2]), r[3] + above + draw::BAND_GAP, page, total, info, style));
    }
    (out, m.bleed && !bleed_distinct)
}

/// The `All` separation (§8.6.6.4): one colorant that paints on every plate, so a mark drawn in
/// it appears on each separation. The tint transform spreads the tint over all four process inks.
fn all_separation() -> Object {
    let mut f = Dict::new();
    f.set(b"FunctionType".to_vec(), Object::Int(2));
    f.set(b"Domain".to_vec(), num_array(&[0.0, 1.0]));
    f.set(b"C0".to_vec(), num_array(&[0.0; 4]));
    f.set(b"C1".to_vec(), num_array(&[1.0; 4]));
    f.set(b"N".to_vec(), Object::Int(1));
    Object::Array(vec![Object::name("Separation"), Object::name("All"), Object::name("DeviceCMYK"), Object::Dict(f)])
}

fn num_array(v: &[f64]) -> Object {
    Object::Array(v.iter().map(|x| Object::Real(*x)).collect())
}

/// Helvetica in WinAnsiEncoding, for the page-information line.
fn helvetica() -> Object {
    let mut d = Dict::new();
    d.set(b"Type".to_vec(), Object::name("Font"));
    d.set(b"Subtype".to_vec(), Object::name("Type1"));
    d.set(b"BaseFont".to_vec(), Object::name("Helvetica"));
    d.set(b"Encoding".to_vec(), Object::name("WinAnsiEncoding"));
    Object::Dict(d)
}

/// The appearance stream of one mark: a form XObject whose `/BBox` is the mark's own rectangle.
fn appearance(mark: &draw::Mark) -> Stream {
    let mut cs = Dict::new();
    cs.set(b"CS0".to_vec(), all_separation());
    let mut res = Dict::new();
    res.set(b"ColorSpace".to_vec(), Object::Dict(cs));
    if mark.font {
        let mut fonts = Dict::new();
        fonts.set(b"Helv".to_vec(), helvetica());
        res.set(b"Font".to_vec(), Object::Dict(fonts));
    }
    let mut d = Dict::new();
    d.set(b"Type".to_vec(), Object::name("XObject"));
    d.set(b"Subtype".to_vec(), Object::name("Form"));
    d.set(b"FormType".to_vec(), Object::Int(1));
    d.set(b"BBox".to_vec(), num_array(&mark.rect));
    d.set(b"Resources".to_vec(), Object::Dict(res));
    Stream::flate(d, &mark.content)
}

/// The entries of a page's `/Annots` (references or inline dictionaries).
fn annots(doc: &Document, page: ObjRef) -> Vec<Object> {
    let obj = doc.get(page);
    let Some(a) = obj.as_dict().and_then(|d| d.get(b"Annots")) else { return Vec::new() };
    doc.resolve(a).as_array().cloned().unwrap_or_default()
}

/// Replace a page's `/Annots`.
///
/// An indirect array this page alone points at is written in place, so the object graph survives
/// the edit. One that another page points at too is left alone and this page is given its own
/// direct array: some producers share one array across every page, and writing into it would put
/// page one's marks on every other page.
fn set_annots(doc: &mut Document, page: ObjRef, list: Vec<Object>) -> Result<()> {
    let existing = doc.get(page).as_dict().and_then(|d| d.reference(b"Annots"));
    let shared = existing.is_some_and(|r| {
        pdfcraft_model::pages(doc).iter().any(|p| p.obj != page && doc.get(p.obj).as_dict().and_then(|d| d.reference(b"Annots")) == Some(r))
    });
    match existing {
        Some(r) if !shared && doc.get(r).as_array().is_some() => doc.set(r, Object::Array(list)),
        _ => doc.update_dict(page, |d| {
            if list.is_empty() {
                d.remove(b"Annots");
            } else {
                d.set(b"Annots".to_vec(), Object::Array(list));
            }
        })?,
    }
    Ok(())
}

/// Whether `entry` is a printer mark PdfCraft added.
fn is_ours(doc: &Document, entry: &Object) -> bool {
    let obj = doc.resolve(entry);
    let Some(d) = obj.as_dict() else { return false };
    d.name(b"Subtype") == Some(b"PrinterMark") && matches!(d.get(PRODUCER_KEY).map(|o| doc.resolve(o)).as_deref(), Some(Object::Bool(true)))
}

/// Add printer marks to `pages_wanted` (0-based). Returns what was added and what did not fit.
///
/// Marks are placed in unrotated user space, so they stay with the page's trim geometry whatever
/// its `/Rotate` is.
pub fn add_printer_marks(doc: &mut Document, pages_wanted: &[usize], m: &Marks) -> Result<Report> {
    if !m.any() {
        return Err(invalid("choose at least one printer mark to add"));
    }
    if let Some(what) = m.style.unusable() {
        return Err(invalid(&format!("{what} is not a usable length")));
    }
    if let Some(c) = m.first_undrawable() {
        return Err(PrepressError::Undrawable(pdfcraft_fonts::undrawable_message("the page information", c)));
    }
    let all = pages(doc)?;
    check(pages_wanted, all.len())?;
    let total = all.len();
    let mut report = Report::default();
    for &index in pages_wanted {
        let Some(page) = all.get(index).cloned() else { continue };
        let b = boxes(doc, &page);
        let (marks, bleed_skipped) = marks_for(index, total, &b, m);
        if bleed_skipped {
            report.no_bleed.push(index.saturating_add(1));
        }
        if marks.is_empty() {
            continue;
        }
        let mut list = annots(doc, page.obj);
        // Measured against both reference boxes. The media box bounds the sheet, but the crop box
        // is what a viewer shows and a RIP cuts to, so a mark inside a large media box and outside
        // a smaller crop box is invisible. `needed` cannot be compared with `b.crop` directly,
        // since it starts from the media box and the crop box is clipped inside it.
        let (mut needed, mut needed_crop) = (b.media, b.crop);
        for mark in &marks {
            needed = union(needed, mark.rect);
            needed_crop = union(needed_crop, mark.rect);
            let ap = doc.add(Object::Stream(appearance(mark)));
            let mut d = Dict::new();
            d.set(b"Type".to_vec(), Object::name("Annot"));
            d.set(b"Subtype".to_vec(), Object::name("PrinterMark"));
            d.set(b"MN".to_vec(), Object::name(mark.name));
            d.set(b"Rect".to_vec(), num_array(&mark.rect));
            d.set(b"F".to_vec(), Object::Int(MARK_FLAGS));
            d.set(PRODUCER_KEY.to_vec(), Object::Bool(true));
            let mut ap_dict = Dict::new();
            ap_dict.set(b"N".to_vec(), Object::Ref(ap));
            d.set(b"AP".to_vec(), Object::Dict(ap_dict));
            list.push(Object::Ref(doc.add(Object::Dict(d))));
            report.added = report.added.saturating_add(1);
        }
        set_annots(doc, page.obj, list)?;
        // A page with no crop box of its own anywhere in the chain has `crop == media`, so growing
        // the media box carries it along and there is nothing else to do.
        let has_crop = page.dict.contains(b"CropBox");
        let (grow_media_box, grow_crop_box) = (needed != b.media, has_crop && needed_crop != b.crop);
        if grow_media_box || grow_crop_box {
            if m.grow_media {
                // The grown boxes go on the page itself, so a shared `/Pages` node keeps the size
                // every other page inherits from it. Each box is saved only if it is being
                // changed, so removal puts back exactly what was touched and nothing else.
                let page_dict = doc.get(page.obj).as_dict().cloned().unwrap_or_default();
                let media_inherited = !page_dict.contains(b"MediaBox");
                let crop_inherited = !page_dict.contains(b"CropBox");
                let (original, original_crop) = (b.media, b.crop);
                doc.update_dict(page.obj, |d| {
                    if grow_media_box {
                        if !d.contains(SAVED_MEDIA_KEY) {
                            d.set(SAVED_MEDIA_KEY.to_vec(), num_array(&original));
                            if media_inherited {
                                d.set(SAVED_MEDIA_INHERITED_KEY.to_vec(), Object::Bool(true));
                            }
                        }
                        d.set(b"MediaBox".to_vec(), num_array(&needed));
                    }
                    if grow_crop_box {
                        if !d.contains(SAVED_CROP_KEY) {
                            d.set(SAVED_CROP_KEY.to_vec(), num_array(&original_crop));
                            if crop_inherited {
                                d.set(SAVED_CROP_INHERITED_KEY.to_vec(), Object::Bool(true));
                            }
                        }
                        // Only as far as the marks reach: the crop the user chose for the artwork
                        // is kept otherwise.
                        d.set(b"CropBox".to_vec(), num_array(&needed_crop));
                    }
                })?;
                report.grown.push(index.saturating_add(1));
            } else {
                report.clipped.push(index.saturating_add(1));
            }
        }
    }
    Ok(report)
}

/// How many printer marks PdfCraft added to `pages_wanted` (0-based) are on the document now.
/// Marks another producer wrote are not counted, since [`remove_printer_marks`] would not take
/// them away either.
pub fn count_printer_marks(doc: &Document, pages_wanted: &[usize]) -> usize {
    let all = pdfcraft_model::pages(doc);
    pages_wanted.iter().filter_map(|i| all.get(*i)).map(|page| annots(doc, page.obj).iter().filter(|e| is_ours(doc, e)).count()).sum()
}

/// Remove the printer marks PdfCraft added to `pages_wanted` (0-based), and put back any media
/// box it enlarged for them. Marks another producer wrote are left alone. Returns how many were
/// removed.
pub fn remove_printer_marks(doc: &mut Document, pages_wanted: &[usize]) -> Result<usize> {
    let all = pages(doc)?;
    check(pages_wanted, all.len())?;
    let mut removed = 0usize;
    for &index in pages_wanted {
        let Some(page) = all.get(index).cloned() else { continue };
        let before = annots(doc, page.obj);
        let kept: Vec<Object> = before.iter().filter(|e| !is_ours(doc, e)).cloned().collect();
        if kept.len() == before.len() {
            continue;
        }
        removed = removed.saturating_add(before.len().saturating_sub(kept.len()));
        set_annots(doc, page.obj, kept)?;
        let dict = doc.get(page.obj).as_dict().cloned().unwrap_or_default();
        let was_inherited = |key: &[u8]| matches!(dict.get(key).map(|o| doc.resolve(o)).as_deref(), Some(Object::Bool(true)));
        let (saved_media, saved_crop) = (rect(doc, dict.get(SAVED_MEDIA_KEY)), rect(doc, dict.get(SAVED_CROP_KEY)));
        if saved_media.is_some() || saved_crop.is_some() {
            // A page that inherited a box has the entry taken away rather than written back: the
            // file had none of its own and must not gain one here (§7.7.3.4).
            let media_inherited = was_inherited(SAVED_MEDIA_INHERITED_KEY);
            let crop_inherited = was_inherited(SAVED_CROP_INHERITED_KEY);
            doc.update_dict(page.obj, |d| {
                for (key, saved, inherited, saved_key, inherited_key) in [
                    (&b"MediaBox"[..], saved_media, media_inherited, SAVED_MEDIA_KEY, SAVED_MEDIA_INHERITED_KEY),
                    (&b"CropBox"[..], saved_crop, crop_inherited, SAVED_CROP_KEY, SAVED_CROP_INHERITED_KEY),
                ] {
                    let Some(original) = saved else { continue };
                    if inherited {
                        d.remove(key);
                    } else {
                        d.set(key.to_vec(), num_array(&original));
                    }
                    d.remove(saved_key);
                    d.remove(inherited_key);
                }
            })?;
        }
    }
    Ok(removed)
}
