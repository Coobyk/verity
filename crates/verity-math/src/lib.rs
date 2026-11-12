//! LaTeX math -> PNG via mitex (TeX->Typst) + typst, callable from Android
//! over JNI (sh.coobyk.verity.math.TypstBridge).

use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jbyteArray, jfloat, jstring};
use jni::JNIEnv;
use once_cell::sync::Lazy;
use std::path::PathBuf;
use std::sync::Mutex;
use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Duration};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_layout::PagedDocument;

/// Bundled fonts — New Computer Modern (the classic TeX look):
/// Math-Book for equations, CM10 regular/italic/bold for \text{} runs.
static FONT_FILES: &[&[u8]] = &[
    include_bytes!("../fonts/NewCMMath-Book.otf"),
    include_bytes!("../fonts/NewCM10-Regular.otf"),
    include_bytes!("../fonts/NewCM10-Italic.otf"),
    include_bytes!("../fonts/NewCM10-Bold.otf"),
];

/// Vendored typst sources — mitex spec package + a per-render body file.
static SPEC_FILES: &[(&str, &str)] = &[
    ("specs/mod.typ", include_str!("../mitex-pkg/mod.typ")),
    ("specs/prelude.typ", include_str!("../mitex-pkg/prelude.typ")),
    ("specs/latex/standard.typ", include_str!("../mitex-pkg/latex/standard.typ")),
];

fn file_id(path: &str) -> FileId {
    RootedPath::new(
        VirtualRoot::Project,
        VirtualPath::new(path).expect("static path"),
    )
    .intern()
}

struct MathWorld {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
    main: FileId,
    sources: std::collections::HashMap<FileId, Source>,
}

impl MathWorld {
    fn new(main_src: String, body_src: String) -> Self {
        let mut book = FontBook::new();
        let mut fonts = Vec::new();
        for bytes in FONT_FILES {
            for font in Font::iter(Bytes::new(*bytes)) {
                book.push(font.info().clone());
                fonts.push(font);
            }
        }
        let mut sources = std::collections::HashMap::new();
        for (path, text) in SPEC_FILES {
            let id = file_id(path);
            sources.insert(id, Source::new(id, (*text).to_string()));
        }
        let body_id = file_id("body.typ");
        sources.insert(body_id, Source::new(body_id, body_src));
        let main = file_id("main.typ");
        sources.insert(main, Source::new(main, main_src));
        Self {
            library: LazyHash::new(Library::builder().build()),
            book: LazyHash::new(book),
            fonts,
            main,
            sources,
        }
    }
}

impl World for MathWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }
    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }
    fn main(&self) -> FileId {
        self.main
    }
    fn source(&self, id: FileId) -> FileResult<Source> {
        self.sources
            .get(&id)
            .cloned()
            .ok_or_else(|| FileError::NotFound(PathBuf::new()))
    }
    fn file(&self, id: FileId) -> FileResult<Bytes> {
        self.sources
            .get(&id)
            .map(|s| Bytes::new(s.text().as_bytes().to_vec()))
            .ok_or_else(|| FileError::NotFound(PathBuf::new()))
    }
    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.get(index).cloned()
    }
    fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
        Datetime::from_ymd(2024, 1, 1)
    }
}

/// Render a LaTeX math expression to PNG bytes.
pub fn render_latex(tex: &str, display: bool, density: f32, dark: bool) -> Result<Vec<u8>, String> {
    let typst_math = mitex::convert_math(tex, Some(mitex_spec_gen::DEFAULT_SPEC.clone()))
        .map_err(|e| format!("mitex: {e}"))?;
    let color = if dark { "white" } else { "black" };
    let main_src = format!(
        "#set page(width: auto, height: auto, margin: 4pt, fill: none)\n\
         #set text(fill: {color}, font: \"New Computer Modern\")\n\
         #show math.equation: set text(font: \"New Computer Modern Math\")\n\
         #import \"specs/mod.typ\": mitex-scope\n\
         #math.equation(block: {display}, eval(read(\"body.typ\"), mode: \"math\", scope: mitex-scope))"
    );
    // body.typ is eval'd as math — raw mitex output, no escaping needed.
    let world = MathWorld::new(main_src, typst_math);
    let warned = typst::compile::<PagedDocument>(&world);
    let doc = warned
        .output
        .map_err(|e| format!("typst: {}", e.iter().map(|d| d.message.to_string()).collect::<Vec<_>>().join("; ")))?;
    let page = doc.pages().first().ok_or("empty doc")?;
    let opts = typst_render::RenderOptions {
        pixel_per_pt: typst::utils::Scalar::new((density.max(1.0) * 1.5) as f64),
        render_bleed: false,
    };
    let pixmap = typst_render::render(page, &opts);
    pixmap.encode_png().map_err(|e| format!("png: {e}"))
}

static LAST_ERR: Lazy<Mutex<String>> = Lazy::new(|| Mutex::new(String::new()));

#[no_mangle]
pub extern "system" fn Java_sh_coobyk_verity_math_TypstBridge_render(
    mut env: JNIEnv,
    _cls: JClass,
    tex: JString,
    display: jboolean,
    density: jfloat,
) -> jbyteArray {
    let tex: String = match env.get_string(&tex) {
        Ok(s) => s.into(),
        Err(_) => return std::ptr::null_mut(),
    };
    match render_latex(&tex, display != 0, density, true) {
        Ok(png) => env
            .byte_array_from_slice(&png)
            .map(|a| a.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        Err(e) => {
            *LAST_ERR.lock().unwrap() = e;
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_sh_coobyk_verity_math_TypstBridge_lastError(
    env: JNIEnv,
    _cls: JClass,
) -> jstring {
    let s = LAST_ERR.lock().unwrap().clone();
    env.new_string(s).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}
