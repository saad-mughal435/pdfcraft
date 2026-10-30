use std::sync::Arc;

use pdfcraft_testkit::Pdf;

use super::*;

/// Two Letter pages. `one` and `two` are extra entries for page 1 and page 2, so a test can give
/// a page its own boxes, rotation or annotations.
fn doc(one: &str, two: &str) -> Document {
    let bytes = Pdf::new()
        .obj("<< /Type /Catalog /Pages 2 0 R >>")
        .obj("<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 612 792] >>")
        .obj(format!("<< /Type /Page /Parent 2 0 R {one} >>"))
        .obj(format!("<< /Type /Page /Parent 2 0 R {two} >>"))
        .build();
    Document::open(Arc::new(bytes)).expect("fixture opens")
}

/// Page 1 has a trim box inset by 18 points and a bleed box inset by 9; page 2 has neither, so
/// its trim and bleed boxes fall back to the crop box, which falls back to the media box.
fn fixture() -> Document {
    doc("/TrimBox [18 18 594 774] /BleedBox [9 9 603 783]", "")
}

const TRIM: Rect = [18.0, 18.0, 594.0, 774.0];
const BLEED: Rect = [9.0, 9.0, 603.0, 783.0];
const MEDIA: Rect = [0.0, 0.0, 612.0, 792.0];

fn page_obj(doc: &Document, page: usize) -> ObjRef {
    pdfcraft_model::pages(doc)[page].obj
}

/// Every annotation dictionary on a page, in `/Annots` order.
fn annot_dicts(doc: &Document, page: usize) -> Vec<Dict> {
    annots(doc, page_obj(doc, page)).iter().filter_map(|e| doc.resolve(e).as_dict().cloned()).collect()
}

fn rect_of(d: &Dict) -> Rect {
    let v: Vec<f64> = d.get(b"Rect").and_then(Object::as_array).unwrap().iter().filter_map(Object::as_f64).collect();
    <[f64; 4]>::try_from(v).unwrap()
}

/// The rectangles of the marks on a page whose `/MN` name is `name`.
fn named(doc: &Document, page: usize, name: &str) -> Vec<Rect> {
    annot_dicts(doc, page).iter().filter(|d| d.name(b"MN") == Some(name.as_bytes())).map(rect_of).collect()
}

/// The decoded appearance stream of the mark at `index`.
fn appearance_of(doc: &Document, page: usize, index: usize) -> (Vec<u8>, Dict) {
    let d = annot_dicts(doc, page).get(index).cloned().unwrap();
    let ap = d.get(b"AP").and_then(Object::as_dict).unwrap().reference(b"N").unwrap();
    match &*doc.get(ap) {
        Object::Stream(s) => (s.decoded().unwrap(), s.dict.clone()),
        other => panic!("the appearance is not a stream: {other:?}"),
    }
}

fn media_of(doc: &Document, page: usize) -> Rect {
    let d = doc.get(page_obj(doc, page)).as_dict().cloned().unwrap();
    rect(doc, d.get(b"MediaBox")).unwrap_or(MEDIA)
}

/// Whether two rectangles share any area.
fn overlaps(a: Rect, b: Rect) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

/// A mark's drawn geometry: its rectangle less the half stroke it carries on every side.
///
/// A corner mark's arms lie on the extensions of the box's own edges, so the stroke is centred
/// on the cut line and half of it falls on the box side of that line. That is what a cut mark is
/// for, and because the arms are offset outside the box, none of the ink lands on the trimmed
/// page. Only the arms' shared bounding rectangle meets the corner, so these checks look at the
/// geometry rather than at that rectangle.
fn geometry(r: Rect, style: &Style) -> Rect {
    let h = style.weight / 2.0;
    [r[0] + h, r[1] + h, r[2] - h, r[3] - h]
}

fn all_marks() -> Marks {
    Marks {
        crop: true,
        bleed: true,
        registration: true,
        color_bars: true,
        info: Some(Info { name: "report.pdf".into(), stamp: "2026-10-17 09:00".into() }),
        ..Marks::default()
    }
}

#[test]
fn boxes_default_to_their_parent_and_clip_to_the_media_box() {
    let d = fixture();
    let pages = pdfcraft_model::pages(&d);
    let first = boxes(&d, &pages[0]);
    assert_eq!((first.media, first.trim, first.bleed), (MEDIA, TRIM, BLEED));
    // No crop box, so it is the media box; trim and bleed are their own.
    assert_eq!((first.crop, first.art), (MEDIA, MEDIA));

    // Page 2 has none of them: everything falls back to the media box.
    let second = boxes(&d, &pages[1]);
    assert_eq!([second.crop, second.bleed, second.trim, second.art], [MEDIA; 4]);

    // A trim box larger than the media box is clipped to it, and the bleed and trim boxes
    // default to the crop box rather than the media box when there is one.
    let d = doc("/CropBox [10 10 500 700] /TrimBox [-50 -50 900 900]", "");
    let b = boxes(&d, &pdfcraft_model::pages(&d)[0]);
    assert_eq!(b.crop, [10.0, 10.0, 500.0, 700.0]);
    assert_eq!(b.trim, MEDIA, "clipped to the media box, not to the crop box");
    assert_eq!(b.bleed, b.crop, "no bleed box of its own, so the crop box");
}

#[test]
fn a_malformed_or_empty_box_is_ignored_rather_than_believed() {
    for entry in [
        "/TrimBox [1 2 3]",           // too short
        "/TrimBox [0 0 0 0]",         // empty
        "/TrimBox [10 10 10 400]",    // zero width
        "/TrimBox (not an array)",    // wrong type
        "/TrimBox [0 0 1e400 1e400]", // not finite
        "/TrimBox [/Name 0 100 100]", // not all numbers
    ] {
        let d = doc(entry, "");
        let b = boxes(&d, &pdfcraft_model::pages(&d)[0]);
        assert_eq!(b.trim, MEDIA, "{entry} should fall back");
    }
    // Reversed corners are ordered, not discarded.
    let d = doc("/TrimBox [594 774 18 18]", "");
    assert_eq!(boxes(&d, &pdfcraft_model::pages(&d)[0]).trim, TRIM);
}

#[test]
fn crop_marks_mark_every_trim_corner_without_reaching_into_the_page() {
    let mut d = fixture();
    let m = Marks { crop: true, ..Marks::default() };
    let report = add_printer_marks(&mut d, &[0], &m).unwrap();
    assert_eq!(report.added, 4);

    let marks = named(&d, 0, "CropMark");
    assert_eq!(marks.len(), 4);
    for r in &marks {
        assert!(!overlaps(geometry(*r, &Style::default()), TRIM), "no ink may land on the trimmed page: {r:?}");
    }
    // One mark per corner, each reaching offset + length away from it.
    let style = Style::default();
    let far = style.offset + style.length;
    let half = style.weight / 2.0;
    let mut corners: Vec<[f64; 2]> = marks
        .iter()
        .map(|r| {
            let x = if (r[0] + half - (TRIM[0] - far)).abs() < 1e-9 { TRIM[0] } else { TRIM[2] };
            let y = if (r[1] + half - (TRIM[1] - far)).abs() < 1e-9 { TRIM[1] } else { TRIM[3] };
            [x, y]
        })
        .collect();
    corners.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(corners, [[TRIM[0], TRIM[1]], [TRIM[0], TRIM[3]], [TRIM[2], TRIM[1]], [TRIM[2], TRIM[3]]]);
}

#[test]
fn bleed_marks_follow_the_bleed_box_and_are_skipped_when_it_matches() {
    // Page 1's bleed box is its own, so the bleed marks sit further out than the crop marks.
    let mut d = fixture();
    let m = Marks { crop: true, bleed: true, ..Marks::default() };
    let report = add_printer_marks(&mut d, &[0], &m).unwrap();
    assert_eq!((report.added, report.no_bleed.as_slice()), (8, [].as_slice()));
    let bleed = named(&d, 0, "BleedMark");
    assert_eq!(bleed.len(), 4);
    for r in &bleed {
        assert!(!overlaps(geometry(*r, &Style::default()), BLEED), "a bleed mark must sit outside the bleed box: {r:?}");
    }

    // Page 2 has no bleed box, so it matches the reference box and the marks would coincide.
    let mut d = fixture();
    let report = add_printer_marks(&mut d, &[1], &m).unwrap();
    assert_eq!(report.no_bleed, vec![2], "reported by the page number a user counts");
    assert_eq!(report.added, 4, "only the crop marks");
    assert!(named(&d, 1, "BleedMark").is_empty());
}

#[test]
fn registration_targets_are_centred_on_each_side_outside_the_box() {
    let mut d = fixture();
    let m = Marks { registration: true, ..Marks::default() };
    add_printer_marks(&mut d, &[0], &m).unwrap();
    let targets = named(&d, 0, "RegistrationTarget");
    assert_eq!(targets.len(), 4);
    let style = Style::default();
    let (mx, my) = ((TRIM[0] + TRIM[2]) / 2.0, (TRIM[1] + TRIM[3]) / 2.0);
    for r in &targets {
        assert!(!overlaps(*r, TRIM), "a target must sit outside the box it registers: {r:?}");
        let centre = [(r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0];
        let on_vertical_side = (centre[0] - mx).abs() < 1e-9;
        assert!(on_vertical_side || (centre[1] - my).abs() < 1e-9, "centred on a side: {centre:?}");
        // The near edge stops exactly at the offset, so the target never eats into the gap.
        let gap = if on_vertical_side {
            if centre[1] < my { TRIM[1] - r[3] } else { r[1] - TRIM[3] }
        } else if centre[0] < mx {
            TRIM[0] - r[2]
        } else {
            r[0] - TRIM[2]
        };
        assert!((gap - style.offset).abs() < 1e-9, "the gap should be the offset, got {gap}");
    }
}

#[test]
fn the_colour_bar_and_page_information_clear_the_corner_marks_and_targets() {
    // The bands are derived from the style, so this must hold for shapes far from the default,
    // including ones where the targets reach further out than the arms do.
    let styles = [
        Style::default(),
        Style { weight: 0.0, length: 1.0, offset: 0.0, radius: 60.0, patch: 40.0, font_size: 24.0 },
        Style { weight: 4.0, length: 200.0, offset: 30.0, radius: 1.0, patch: 2.0, font_size: 1.0 },
        Style { weight: 0.5, length: 18.0, offset: 0.0, radius: 9.0, patch: 12.0, font_size: 7.0 },
    ];
    for style in styles {
        let mut d = fixture();
        let m = Marks { style, ..all_marks() };
        add_printer_marks(&mut d, &[0], &m).unwrap();
        let arms: Vec<Rect> = ["CropMark", "BleedMark", "RegistrationTarget"].iter().flat_map(|n| named(&d, 0, n)).collect();
        let late: Vec<Rect> = ["ColorBar", "PageInformation"].iter().flat_map(|n| named(&d, 0, n)).collect();
        assert_eq!(late.len(), 2);
        for l in &late {
            for a in &arms {
                assert!(!overlaps(*l, *a), "{style:?}: {l:?} overlaps {a:?}");
            }
            assert!(!overlaps(*l, TRIM), "{style:?}: {l:?} reaches into the page");
        }
    }
}

#[test]
fn page_information_numbers_pages_from_one_and_names_the_document() {
    let mut d = fixture();
    let m = Marks { info: Some(Info { name: "quarterly.pdf".into(), stamp: "2026-10-17".into() }), ..Marks::default() };
    add_printer_marks(&mut d, &[0, 1], &m).unwrap();
    for (page, expected) in [(0usize, "Page 1 of 2"), (1, "Page 2 of 2")] {
        let (content, _) = appearance_of(&d, page, 0);
        let text = String::from_utf8_lossy(&content).to_string();
        assert!(text.contains(expected), "page {page}: {text}");
        assert!(text.contains("(quarterly.pdf)"), "page {page}: {text}");
        assert!(text.contains("(2026-10-17)"), "page {page}: {text}");
    }
}

#[test]
fn page_information_the_standard_fonts_cannot_draw_is_refused_not_mangled() {
    let mut d = fixture();
    for info in [Info { name: "Dvořák.pdf".into(), stamp: String::new() }, Info { name: String::new(), stamp: "日付".into() }] {
        let m = Marks { info: Some(info), ..Marks::default() };
        assert!(m.first_undrawable().is_some());
        let err = add_printer_marks(&mut d, &[0], &m).unwrap_err();
        assert!(matches!(err, PrepressError::Undrawable(_)), "{err:?}");
        assert!(err.to_string().contains("the page information"), "{err}");
    }
    assert!(annot_dicts(&d, 0).is_empty(), "a refusal must add nothing");
    // Characters WinAnsi does have are fine.
    let m = Marks { info: Some(Info { name: "Café €5.pdf".into(), stamp: String::new() }), ..Marks::default() };
    assert_eq!(m.first_undrawable(), None);
    assert!(add_printer_marks(&mut d, &[0], &m).is_ok());
}

#[test]
fn nothing_selected_or_an_unusable_measurement_is_refused() {
    let mut d = fixture();
    let err = add_printer_marks(&mut d, &[0], &Marks::default()).unwrap_err();
    assert_eq!(err, invalid("choose at least one printer mark to add"));

    for (style, what) in [
        (Style { length: 0.0, ..Style::default() }, "the mark length"),
        (Style { length: f64::NAN, ..Style::default() }, "the mark length"),
        (Style { offset: f64::INFINITY, ..Style::default() }, "the mark offset"),
        (Style { offset: -1.0, ..Style::default() }, "the mark offset"),
        (Style { weight: f64::NAN, ..Style::default() }, "the line weight"),
        (Style { radius: MAX_MEASURE + 1.0, ..Style::default() }, "the target radius"),
        (Style { patch: -0.0001, ..Style::default() }, "the patch size"),
        (Style { font_size: 0.0, ..Style::default() }, "the text size"),
    ] {
        let m = Marks { crop: true, style, ..Marks::default() };
        let err = add_printer_marks(&mut d, &[0], &m).unwrap_err();
        assert_eq!(err, invalid(&format!("{what} is not a usable length")), "{style:?}");
    }
    // A zero weight and a zero offset are both meaningful, so they are allowed.
    let m = Marks { crop: true, style: Style { weight: 0.0, offset: 0.0, ..Style::default() }, ..Marks::default() };
    assert!(add_printer_marks(&mut d, &[0], &m).is_ok());
    assert!(annot_dicts(&d, 0).len() == 4);
}

#[test]
fn a_page_past_the_end_is_refused_by_the_number_a_user_counts() {
    let mut d = fixture();
    let m = Marks { crop: true, ..Marks::default() };
    let err = add_printer_marks(&mut d, &[0, 2], &m).unwrap_err();
    assert_eq!(err, PrepressError::NoSuchPage(2));
    assert_eq!(err.to_string(), "page 3 does not exist");
    assert!(annot_dicts(&d, 0).is_empty(), "the whole selection is checked before anything is written");
    assert_eq!(remove_printer_marks(&mut d, &[5]).unwrap_err(), PrepressError::NoSuchPage(5));
}

#[test]
fn growing_the_media_box_fits_every_mark_and_removal_puts_it_back() {
    let mut d = fixture();
    let m = Marks { grow_media: true, ..all_marks() };
    let report = add_printer_marks(&mut d, &[0], &m).unwrap();
    assert_eq!((report.grown.as_slice(), report.clipped.as_slice()), ([1].as_slice(), [].as_slice()));

    let grown = media_of(&d, 0);
    assert!(grown != MEDIA, "the media box should have been enlarged");
    for r in annot_dicts(&d, 0).iter().map(rect_of) {
        assert!(
            r[0] >= grown[0] - 1e-9 && r[1] >= grown[1] - 1e-9 && r[2] <= grown[2] + 1e-9 && r[3] <= grown[3] + 1e-9,
            "{r:?} is outside the grown media box {grown:?}"
        );
    }
    // The trim box is what it was: growing the sheet must not move the cut.
    assert_eq!(boxes(&d, &pdfcraft_model::pages(&d)[0]).trim, TRIM);

    assert_eq!(remove_printer_marks(&mut d, &[0]).unwrap(), report.added);
    assert_eq!(media_of(&d, 0), MEDIA, "removal restores the original media box");
    assert!(annot_dicts(&d, 0).is_empty());
    assert!(!doc_has_key(&d, 0, SAVED_MEDIA_KEY), "the saved media box is cleared once it has been put back");
}

#[test]
fn growing_an_inherited_media_box_leaves_the_other_pages_and_the_file_as_they_were() {
    // Both pages inherit one media box from the shared /Pages node. Growing page 1 must write
    // its own box, leave page 2 the size it inherits, and on removal take the entry away again
    // rather than leaving page 1 with a direct box the file never had.
    let mut d = fixture();
    assert!(!doc_has_key(&d, 0, b"MediaBox"), "the fixture's pages inherit their size");
    let report = add_printer_marks(&mut d, &[0], &Marks { grow_media: true, ..all_marks() }).unwrap();
    assert_eq!(report.grown, vec![1]);
    assert!(doc_has_key(&d, 0, b"MediaBox"), "the grown page gets its own box");
    assert!(doc_has_key(&d, 0, SAVED_MEDIA_INHERITED_KEY), "recorded as inherited");
    assert_eq!(media_of(&d, 1), MEDIA, "page 2 still inherits the original size");
    assert!(!doc_has_key(&d, 1, b"MediaBox"));

    remove_printer_marks(&mut d, &[0]).unwrap();
    assert!(!doc_has_key(&d, 0, b"MediaBox"), "the entry is gone, so the size is inherited again");
    assert!(!doc_has_key(&d, 0, SAVED_MEDIA_INHERITED_KEY));
    assert!(!doc_has_key(&d, 0, SAVED_MEDIA_KEY));
    assert_eq!(media_of(&d, 0), MEDIA);

    // A page with a media box of its own gets that exact box back, not a removal.
    let mut d = doc("/MediaBox [0 0 400 500] /TrimBox [100 100 300 400]", "");
    add_printer_marks(&mut d, &[0], &Marks { grow_media: true, ..all_marks() }).unwrap();
    assert!(!doc_has_key(&d, 0, SAVED_MEDIA_INHERITED_KEY), "it had its own box");
    remove_printer_marks(&mut d, &[0]).unwrap();
    assert!(doc_has_key(&d, 0, b"MediaBox"), "its own box is kept");
    assert_eq!(media_of(&d, 0), [0.0, 0.0, 400.0, 500.0]);
}

fn doc_has_key(d: &Document, page: usize, key: &[u8]) -> bool {
    d.get(page_obj(d, page)).as_dict().is_some_and(|p| p.contains(key))
}

#[test]
fn without_growing_the_marks_that_fall_outside_are_reported_not_dropped() {
    let mut d = fixture();
    let report = add_printer_marks(&mut d, &[0], &all_marks()).unwrap();
    assert_eq!(report.clipped, vec![1], "reported by the page number a user counts");
    assert!(report.grown.is_empty());
    assert_eq!(media_of(&d, 0), MEDIA, "the media box is left exactly as it was");
    assert_eq!(annot_dicts(&d, 0).len(), report.added, "every mark is still written");

    // A page with room for the marks needs no growing and reports nothing.
    let mut d = doc("/TrimBox [100 100 512 692]", "");
    let report = add_printer_marks(&mut d, &[0], &Marks { crop: true, ..Marks::default() }).unwrap();
    assert_eq!((report.clipped.as_slice(), report.grown.as_slice()), ([].as_slice(), [].as_slice()));
}

#[test]
fn removal_keeps_printer_marks_another_producer_wrote() {
    // Page 1 already carries a printer mark without our key, and an unrelated comment.
    let mut d = doc(
        "/TrimBox [18 18 594 774] /Annots [\
         << /Type /Annot /Subtype /PrinterMark /MN /ColorBar /Rect [0 0 10 10] >> \
         << /Type /Annot /Subtype /Square /Rect [20 20 60 60] >>]",
        "",
    );
    let before = annot_dicts(&d, 0).len();
    assert_eq!(before, 2);
    let added = add_printer_marks(&mut d, &[0], &Marks { crop: true, ..Marks::default() }).unwrap().added;
    assert_eq!(annot_dicts(&d, 0).len(), before + added);

    assert_eq!(remove_printer_marks(&mut d, &[0]).unwrap(), added);
    let left = annot_dicts(&d, 0);
    assert_eq!(left.len(), 2, "the other producer's mark and the comment both survive");
    assert_eq!(left[0].name(b"Subtype"), Some(b"PrinterMark".as_slice()));
    assert_eq!(left[1].name(b"Subtype"), Some(b"Square".as_slice()));
    // Removing again is a no-op rather than an error.
    assert_eq!(remove_printer_marks(&mut d, &[0, 1]).unwrap(), 0);
}

/// An indirect `/Annots` one page owns is written in place, so the object graph survives. One that
/// two pages share is not: the second page must not pick up the first page's marks. The earlier
/// version of this test was named for the shared case and did not build it, since the second page
/// had no `/Annots` at all.
#[test]
fn marks_go_into_a_shared_annots_array_without_reaching_the_other_page() {
    // Both pages really do point at the same indirect array, as files from some producers do.
    let shared = || {
        Pdf::new()
            .obj("<< /Type /Catalog /Pages 2 0 R >>")
            .obj("<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 612 792] >>")
            .obj("<< /Type /Page /Parent 2 0 R /Annots 5 0 R /TrimBox [100 100 512 692] >>")
            .obj("<< /Type /Page /Parent 2 0 R /Annots 5 0 R /TrimBox [100 100 512 692] >>")
            .obj("[]")
            .build()
    };
    let mut d = Document::open(Arc::new(shared())).expect("opens");
    add_printer_marks(&mut d, &[0], &Marks { crop: true, ..Marks::default() }).unwrap();
    assert_eq!(annot_dicts(&d, 0).len(), 4, "page 1 has its marks");
    assert!(annot_dicts(&d, 1).is_empty(), "page 2 shares the array and must not have gained them");
    assert_eq!(doc_dict(&d, 1).reference(b"Annots"), Some(ObjRef::new(5, 0)), "page 2 still points at the shared array");
    assert!(doc_dict(&d, 0).reference(b"Annots").is_none(), "page 1 took its own array instead of overwriting the shared one");
    assert_eq!(remove_printer_marks(&mut d, &[0]).unwrap(), 4);
    assert!(annot_dicts(&d, 0).is_empty() && annot_dicts(&d, 1).is_empty());

    // An array only one page points at is still edited in place, which is the case the old test
    // was really covering.
    let mut d = Document::open(Arc::new(
        Pdf::new()
            .obj("<< /Type /Catalog /Pages 2 0 R >>")
            .obj("<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 612 792] >>")
            .obj("<< /Type /Page /Parent 2 0 R /Annots 5 0 R /TrimBox [100 100 512 692] >>")
            .obj("<< /Type /Page /Parent 2 0 R >>")
            .obj("[]")
            .build(),
    ))
    .expect("opens");
    add_printer_marks(&mut d, &[0], &Marks { crop: true, ..Marks::default() }).unwrap();
    assert_eq!(doc_dict(&d, 0).reference(b"Annots"), Some(ObjRef::new(5, 0)), "the array object itself was updated");
    assert_eq!(annot_dicts(&d, 0).len(), 4);
    assert_eq!(remove_printer_marks(&mut d, &[0]).unwrap(), 4);
    assert_eq!(doc_dict(&d, 0).reference(b"Annots"), Some(ObjRef::new(5, 0)), "the unshared array is kept");
    assert!(annot_dicts(&d, 0).is_empty());
}

fn doc_dict(d: &Document, page: usize) -> Dict {
    d.get(page_obj(d, page)).as_dict().cloned().unwrap_or_default()
}

#[test]
fn every_mark_prints_read_only_and_draws_through_the_all_separation() {
    let mut d = fixture();
    add_printer_marks(&mut d, &[0], &all_marks()).unwrap();
    let dicts = annot_dicts(&d, 0);
    assert!(!dicts.is_empty());
    for (i, a) in dicts.iter().enumerate() {
        assert_eq!(a.name(b"Subtype"), Some(b"PrinterMark".as_slice()));
        assert_eq!(a.get(b"F").and_then(Object::as_int), Some(MARK_FLAGS), "Print and ReadOnly");
        assert!(a.name(b"MN").is_some(), "the spec's mark name");
        assert_eq!(a.get(PRODUCER_KEY), Some(&Object::Bool(true)));

        let (content, form) = appearance_of(&d, 0, i);
        assert_eq!(form.name(b"Subtype"), Some(b"Form".as_slice()));
        // The form's box is the annotation's, so the mark draws in page coordinates.
        let bbox: Vec<f64> = form.get(b"BBox").and_then(Object::as_array).unwrap().iter().filter_map(Object::as_f64).collect();
        assert_eq!(bbox, rect_of(a).to_vec());
        assert!(!content.is_empty(), "a mark with no operators would draw nothing");

        let resources = form.get(b"Resources").and_then(Object::as_dict).unwrap();
        let cs = resources.get(b"ColorSpace").and_then(Object::as_dict).unwrap();
        let separation = cs.get(b"CS0").and_then(Object::as_array).unwrap();
        assert_eq!(separation.first(), Some(&Object::name("Separation")));
        assert_eq!(separation.get(1), Some(&Object::name("All")), "paints on every plate");
        assert_eq!(separation.get(2), Some(&Object::name("DeviceCMYK")));

        // Only the colour bar uses process inks directly; the rest draw in the All separation.
        let text = String::from_utf8_lossy(&content).to_string();
        if a.name(b"MN") == Some(b"ColorBar") {
            assert!(text.contains(" k "), "the bar's patches are DeviceCMYK: {text}");
        } else {
            assert!(text.contains("/CS0"), "{text}");
        }
        assert_eq!(resources.contains(b"Font"), a.name(b"MN") == Some(b"PageInformation"));
    }
}

#[test]
fn the_reference_box_chooses_what_is_marked() {
    for (which, expected) in [(RefBox::Trim, TRIM), (RefBox::Bleed, BLEED), (RefBox::Media, MEDIA)] {
        let mut d = fixture();
        let m = Marks { crop: true, reference: which, ..Marks::default() };
        add_printer_marks(&mut d, &[0], &m).unwrap();
        for r in named(&d, 0, "CropMark") {
            assert!(!overlaps(geometry(r, &Style::default()), expected), "{which:?}: {r:?}");
            // A corner of the chosen box is a corner of the mark's rectangle, within the stroke.
            let half = Style::default().weight / 2.0;
            let touches = [expected[0], expected[2]].iter().any(|x| (r[0] + half - x).abs() < 1e-9 || (r[2] - half - x).abs() < 1e-9);
            assert!(touches, "{which:?}: {r:?} does not meet a corner of {expected:?}");
        }
    }
    assert_eq!(RefBox::from_name("TrimBox"), Some(RefBox::Trim));
    assert_eq!(RefBox::from_name("bleed"), Some(RefBox::Bleed));
    assert_eq!(RefBox::from_name("nonsense"), None);
    assert_eq!(RefBox::default().key(), b"TrimBox");
}

#[test]
fn a_document_without_pages_is_an_error_and_never_a_panic() {
    let bytes = Pdf::new().obj("<< /Type /Catalog >>").build();
    let mut d = Document::open(Arc::new(bytes)).expect("opens");
    let m = Marks { crop: true, ..Marks::default() };
    assert_eq!(add_printer_marks(&mut d, &[], &m).unwrap_err(), PrepressError::NoPageTree);
    assert_eq!(remove_printer_marks(&mut d, &[]).unwrap_err(), PrepressError::NoPageTree);
}

#[test]
fn hostile_pages_and_an_empty_selection_never_panic() {
    // Huge, reversed, degenerate and non-numeric boxes, a rotated page, and marks asked for on
    // no pages at all: every one of these has to come back as a value, not a panic.
    for entry in [
        "/MediaBox [0 0 1e9 1e9] /TrimBox [1 1 1e9 1e9]",
        "/MediaBox [-1e9 -1e9 1e9 1e9]",
        "/MediaBox [0 0 1 1] /TrimBox [0 0 1 1]",
        "/MediaBox [0 0 612 792] /Rotate 90 /TrimBox [18 18 594 774]",
        "/MediaBox [0 0 612 792] /Rotate 37",
        "/MediaBox (nonsense) /TrimBox [/a /b /c /d]",
        "/MediaBox [0 0 612 792] /Annots 99 0 R",
        "/MediaBox [0 0 612 792] /Annots [null 1 2 (x)]",
    ] {
        let mut d = doc(entry, "");
        for grow in [false, true] {
            let m = Marks { grow_media: grow, ..all_marks() };
            let _ = add_printer_marks(&mut d, &[0, 1], &m);
            let _ = remove_printer_marks(&mut d, &[0, 1]);
        }
        let m = Marks { crop: true, ..Marks::default() };
        assert_eq!(add_printer_marks(&mut d, &[], &m).unwrap(), Report::default(), "{entry}");
        assert_eq!(remove_printer_marks(&mut d, &[]).unwrap(), 0, "{entry}");
    }
}

#[test]
fn a_rotated_page_keeps_its_marks_on_the_trim_geometry() {
    // Marks are placed in unrotated user space, so /Rotate does not move them (README: the
    // page-information text turning with the page is a known limit).
    let mut upright = doc("/TrimBox [18 18 594 774]", "");
    let mut turned = doc("/Rotate 90 /TrimBox [18 18 594 774]", "");
    let m = Marks { crop: true, registration: true, ..Marks::default() };
    add_printer_marks(&mut upright, &[0], &m).unwrap();
    add_printer_marks(&mut turned, &[0], &m).unwrap();
    for name in ["CropMark", "RegistrationTarget"] {
        assert_eq!(named(&upright, 0, name), named(&turned, 0, name), "{name}");
    }
}

#[test]
fn adding_marks_twice_adds_a_second_set_that_removal_clears_in_one_go() {
    let mut d = fixture();
    let m = Marks { crop: true, ..Marks::default() };
    let first = add_printer_marks(&mut d, &[0], &m).unwrap().added;
    let second = add_printer_marks(&mut d, &[0], &m).unwrap().added;
    assert_eq!(annot_dicts(&d, 0).len(), first + second);
    assert_eq!(remove_printer_marks(&mut d, &[0]).unwrap(), first + second);
    assert!(annot_dicts(&d, 0).is_empty());
}

#[test]
fn the_number_formatter_writes_short_stable_pdf_syntax() {
    assert_eq!(draw::n(0.0), "0");
    assert_eq!(draw::n(-0.0), "0");
    assert_eq!(draw::n(-0.0001), "0", "below the threshold, so no negative zero reaches the file");
    assert_eq!(draw::n(18.0), "18");
    assert_eq!(draw::n(0.25), "0.25");
    assert_eq!(draw::n(-6.125), "-6.125");
    assert_eq!(draw::n(1.0 / 3.0), "0.333");
}

fn crop_of(doc: &Document, page: usize) -> Option<Rect> {
    let d = doc.get(page_obj(doc, page)).as_dict().cloned().unwrap_or_default();
    rect(doc, d.get(b"CropBox"))
}

/// The crop box is what a viewer shows and a RIP cuts to, so a mark inside a large media box and
/// outside a smaller crop box is invisible. Growing the page has to grow both, and removal has to
/// put both back. Before this, marks were measured against the media box alone: a page with an
/// explicit crop box reported neither clipped nor grown and the marks were simply not shown.
#[test]
fn an_explicit_crop_box_is_grown_with_the_media_box_and_put_back() {
    // Letter crop inside a larger sheet, so every mark lands outside the crop box.
    // The sheet fits every mark; the crop box does not. So the old code, which looked only at the
    // media box, reported neither clipped nor grown and the marks were simply never shown.
    let page = "/MediaBox [0 0 1200 1400] /CropBox [100 100 712 892] /TrimBox [120 120 692 872]";

    // Without grow_media the page is reported clipped, which it is: the marks will not be seen.
    let mut d = doc(page, "");
    let report = add_printer_marks(&mut d, &[0], &all_marks()).unwrap();
    assert_eq!(report.clipped, vec![1], "marks outside the crop box are clipped even though the media box fits them");
    assert_eq!(crop_of(&d, 0), Some([100.0, 100.0, 712.0, 892.0]), "and nothing was changed");

    // With it, both boxes grow and every mark is inside the crop box, not merely inside the sheet.
    let mut d = doc(page, "");
    let report = add_printer_marks(&mut d, &[0], &Marks { grow_media: true, ..all_marks() }).unwrap();
    assert_eq!((report.grown.as_slice(), report.clipped.as_slice()), ([1].as_slice(), [].as_slice()));
    let crop = crop_of(&d, 0).expect("the page still has a crop box");
    assert!(crop != [100.0, 100.0, 712.0, 892.0], "the crop box was enlarged");
    for r in annot_dicts(&d, 0).iter().map(rect_of) {
        assert!(
            r[0] >= crop[0] - 1e-9 && r[1] >= crop[1] - 1e-9 && r[2] <= crop[2] + 1e-9 && r[3] <= crop[3] + 1e-9,
            "{r:?} is outside the grown crop box {crop:?}"
        );
    }
    // Only as far as the marks reach: the crop stays inside the grown sheet.
    let media = media_of(&d, 0);
    assert!(crop[2] <= media[2] + 1e-9 && crop[3] <= media[3] + 1e-9, "crop {crop:?} outgrew media {media:?}");

    assert_eq!(remove_printer_marks(&mut d, &[0]).unwrap(), report.added);
    assert_eq!(crop_of(&d, 0), Some([100.0, 100.0, 712.0, 892.0]), "removal puts the crop box back");
    assert_eq!(media_of(&d, 0), [0.0, 0.0, 1200.0, 1400.0], "and the media box, which never needed growing");
    assert!(!doc_has_key(&d, 0, SAVED_CROP_KEY), "the saved crop box is cleared");
    assert!(!doc_has_key(&d, 0, SAVED_MEDIA_KEY), "and no media box was saved, since none was changed");

    // A page with no crop box anywhere keeps having none: crop follows media, so there is nothing
    // to grow or to save.
    let mut d = fixture();
    add_printer_marks(&mut d, &[0], &Marks { grow_media: true, ..all_marks() }).unwrap();
    assert_eq!(crop_of(&d, 0), None, "no crop box was invented");
    assert!(!doc_has_key(&d, 0, SAVED_CROP_KEY));
}
