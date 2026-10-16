//! File › Print: the pages go to a printer through the system print spooler (craft-print), as PDF.

use serde_json::{Value, json};

#[cfg(not(target_arch = "wasm32"))]
use super::str_param;
use super::{CommandSpec, bad, cmd, has_doc};
#[cfg(not(target_arch = "wasm32"))]
use crate::EngineError;
use crate::{Result, Session};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "file.printers", "Printers", [], None, "{} → {printers: [names], default}", super::always, |_, _| Ok(printers())),
        cmd!(
            noundo "file.print",
            "Print…",
            ["File"],
            Some("Cmd+P"),
            "{printer?, copies?: 1, pages?: \"1-3,5\", spreads?, marks?, bleed?, dryRun?: bool (export and preview submission without printing)} → {printer, copies, pages, command, bytes}",
            has_doc,
            print
        ),
        cmd!(
            noundo "file.printNetwork",
            "Print to a Network Printer",
            [],
            None,
            "{address: \"192.168.0.9\" | \"ipp://192.168.0.9/ipp/print\", user_confirmed: true, pages?, spreads?, marks?, bleed?, grayscale?, copies?: 1, waitSeconds?: 60} → {printer, address, outcome, pages, warnings}; user_confirmed must be true and may be set only after the person has agreed to this print",
            has_doc,
            print_network
        ),
    ]
}

fn printers() -> Value {
    let printers = craft_print::printers();
    let default = printers.iter().find(|p| p.default).map(|p| p.name.clone());
    let names: Vec<_> = printers.into_iter().map(|p| p.name).collect();
    json!({"printers": names, "default": default})
}

fn print(s: &mut Session, p: &Value) -> Result<Value> {
    #[cfg(target_arch = "wasm32")]
    {
        super::export::options(p, s.doc()?.doc.page_count())?;
        Err(bad("file.print", "printing isn't available on the web: export a PDF and print it"))
    }
    #[cfg(not(target_arch = "wasm32"))]
    print_with(s, p, craft_print::submit)
}

/// The print-ready PDF of the pages a print would send (`pages`, `spreads`, `marks`, `bleed` as in
/// `file.print`), for a destination the host sends itself: a network printer's worker runs off the
/// session's thread, so it takes the bytes rather than the document.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
pub struct PrintDocument {
    pub pdf: Vec<u8>,
    /// How many pages (or spreads) the PDF has.
    pub pages: usize,
    /// The document's title, for the job's name.
    pub title: String,
}

/// Export the active document as a print-ready PDF (see [`PrintDocument`]). Sends nothing.
#[cfg(not(target_arch = "wasm32"))]
pub fn print_document(s: &mut Session, p: &Value) -> Result<PrintDocument> {
    let d = &s.doc()?.doc;
    let opts = print_options(p, d.page_count())?;
    let mut pdf = designcraft_pdf::export_pdf(d, &s.cache, &opts).map_err(|e| EngineError::Other(e.to_string()))?;
    if p.get("grayscale").and_then(Value::as_bool).unwrap_or(false) {
        pdf = grayscale_pdf(&pdf, Some(&d.title))?;
    }
    let pages = opts.pages.as_ref().map_or(d.page_count(), |v| v.len());
    Ok(PrintDocument { pdf, pages, title: d.title.clone() })
}

/// The export options of a print: the user's page selection, spreads, marks and bleed, as a print-only
/// export (no annotations: links, buttons, form fields and media don't print, and the Windows spooler
/// path refuses to fit a page that carries any).
#[cfg(not(target_arch = "wasm32"))]
fn print_options(p: &Value, page_count: usize) -> Result<designcraft_pdf::PdfOptions> {
    Ok(designcraft_pdf::PdfOptions { print: true, ..super::export::options(p, page_count)? })
}

/// Resolution the gray pages are composed at.
#[cfg(not(target_arch = "wasm32"))]
const GRAY_DPI: f64 = 300.0;

/// The print-ready PDF with every page converted to Rec. 709 gray (rendered at 300 dpi over white
/// and stored as one gray picture per page), so a "Grayscale" print is gray whatever the driver does.
#[cfg(not(target_arch = "wasm32"))]
pub fn grayscale_pdf(pdf: &[u8], title: Option<&str>) -> Result<Vec<u8>> {
    let count =
        designcraft_render::pdf_page_count(pdf).filter(|n| *n > 0).ok_or_else(|| EngineError::Other("no pages to convert to grayscale".into()))?;
    let mut pages = Vec::with_capacity(count);
    for i in 0..count {
        let (w, h) = designcraft_render::pdf_page_size(pdf, i).ok_or_else(|| EngineError::Other(format!("page {} could not be read", i + 1)))?;
        let side = (w.max(h) * GRAY_DPI / 72.0).round().clamp(1.0, 8000.0) as u32;
        let pm =
            designcraft_render::render_pdf_page(pdf, i, side).ok_or_else(|| EngineError::Other(format!("page {} could not be rendered", i + 1)))?;
        let pixels: Vec<u8> = pm
            .data()
            .iter()
            .map(|p| {
                // Premultiplied RGBA over white.
                let over = |c: u8| (u16::from(c) + (255 - u16::from(p.a))).min(255) as u8;
                designcraft_pdf::rec709_luma(over(p.r), over(p.g), over(p.b))
            })
            .collect();
        pages.push(designcraft_pdf::GrayPage { points: (w as f32, h as f32), size: (u32::from(pm.width()), u32::from(pm.height())), pixels });
    }
    designcraft_pdf::gray_pdf(&pages, title).map_err(|e| EngineError::Other(e.to_string()))
}

#[cfg(not(target_arch = "wasm32"))]
fn print_with(
    s: &mut Session,
    p: &Value,
    submit: impl FnOnce(&[u8], &craft_print::Job) -> std::result::Result<String, craft_print::Error>,
) -> Result<Value> {
    let d = &s.doc()?.doc;
    let opts = print_options(p, d.page_count())?;
    let grayscale = p.get("grayscale").and_then(Value::as_bool).unwrap_or(false);
    let copies = p.get("copies").and_then(Value::as_u64).unwrap_or(1).clamp(1, 999) as u32;
    let job = craft_print::Job {
        printer: str_param(p, "printer").map(str::to_string),
        copies,
        // The queue's own default paper, the page shrunk to fit it: a document's own page size
        // (a slide, an artboard) is not a paper the queue has to list. No media is ever demanded.
        scaling: craft_print::Scaling::ShrinkToFit,
        grayscale,
        title: d.title.clone(),
        ..craft_print::Job::default()
    };
    let dry = p.get("dryRun").and_then(Value::as_bool).unwrap_or(false);
    let mut pdf =
        designcraft_pdf::export_pdf(&super::interchange::with_pdf_boxes(d), &s.cache, &opts).map_err(|e| EngineError::Other(e.to_string()))?;
    if grayscale && !dry {
        pdf = grayscale_pdf(&pdf, Some(&d.title))?;
    }
    let command = craft_print::command_preview(&job).map_err(|e| bad("file.print", e.to_string()))?;
    let pages = opts.pages.as_ref().map_or(d.page_count(), |v| v.len());
    let out = json!({"printer": job.printer, "copies": copies, "pages": pages, "command": command, "bytes": pdf.len()});
    if !dry {
        submit(&pdf, &job).map_err(|e| bad("file.print", e.to_string()))?;
    }
    Ok(out)
}

/// File › Print to a network printer (WS-Print or IPP) by address, for scripts and agents: the same
/// path as the Print dialog's Network printer destination, without a dialog.
///
/// * **Consent.** `user_confirmed` must be `true` and may be set only after the person has agreed to
///   this print; without it nothing is exported, opened or sent (`UserConfirmed::attest`).
/// * **Address.** A numeric loopback, link-local or private address; no host names, no TLS, no
///   credentials, no discovery (the registry's network policy). Nothing is listed or searched for.
/// * **Sends once.** A send whose outcome is unknown is an error that says to check the printer;
///   nothing retries it.
#[cfg(not(target_arch = "wasm32"))]
fn print_network(s: &mut Session, p: &Value) -> Result<Value> {
    print_network_with(s, p, None)
}

#[cfg(target_arch = "wasm32")]
fn print_network(_: &mut Session, _: &Value) -> Result<Value> {
    Err(bad("file.printNetwork", "network printing isn't available on the web: export a PDF and print it"))
}

#[cfg(not(target_arch = "wasm32"))]
fn print_network_with(s: &mut Session, p: &Value, transport: Option<designcraft_print::network::Transport>) -> Result<Value> {
    use std::time::Duration;

    use designcraft_print::network as net;
    const ID: &str = "file.printNetwork";
    let fail = |e: net::PrintError| {
        let note = if e.outcome_unknown() { format!(" ({})", net::CHECK_THE_PRINTER) } else { String::new() };
        bad(ID, format!("{e}{note}"))
    };
    let address = str_param(p, "address")
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| bad(ID, "address is required, for example 192.168.0.9 or ipp://192.168.0.9/ipp/print"))?;
    // Consent comes first: without it nothing is exported, opened or sent.
    let confirmed = net::UserConfirmed::attest(p.get("user_confirmed").and_then(Value::as_bool)).map_err(fail)?;
    let address = if address.contains("://") { address.to_owned() } else { format!("http://{address}") };
    net::validate_address(&address).map_err(fail)?;
    let wait = p.get("waitSeconds").and_then(Value::as_u64).unwrap_or(60).clamp(1, 600);
    let copies = p.get("copies").and_then(Value::as_u64).unwrap_or(1).clamp(1, 999) as u32;
    let grayscale = p.get("grayscale").and_then(Value::as_bool).unwrap_or(false);

    let doc = print_document(s, p)?;
    net::register_pdf_converter();
    let printer = net::Printer::open(&address, transport).map_err(fail)?;
    // Ask the printer first, as the dialog's Query does; the job is decided on what it said.
    let info = printer.info().map_err(fail)?;
    printer.remember(&info);

    let title: String = doc.title.chars().filter(|c| !c.is_control()).take(120).collect();
    let mut job = net::Job::default();
    if !title.trim().is_empty() {
        job.title = title;
    }
    job.copies = copies;
    job.scaling = net::Scaling::ShrinkToFit;
    let mut options = net::JobOptions::new();
    if let Ok(pages) = u32::try_from(doc.pages) {
        options = options.with_expected_pages(pages);
    }
    if grayscale {
        options = options.with_colour(net::ColourMode::Mono);
    }
    let cancel = net::CancelToken::new();
    let sent = printer.submit(net::Document::pdf(&doc.pdf), job, options, confirmed, &cancel, &mut |_| {}).map_err(fail)?;

    // Follow the accepted job to its end (or `waitSeconds`).
    let policy =
        net::WaitPolicy { interval: Some(Duration::from_millis(500)), ..net::WaitPolicy::default() }.with_max_wait(Duration::from_secs(wait));
    let outcome = match printer.watch(&sent, policy) {
        Ok(mut watch) => loop {
            match watch.tick(printer.source()) {
                net::WatchStep::Pending { next_in } => std::thread::sleep(next_in.min(Duration::from_millis(100))),
                net::WatchStep::Done(outcome) => break outcome,
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        },
        Err(e) => net::WatchOutcome::Failed(e),
    };
    let name = net::outcome_name(&outcome);
    let printer_name = sent.printer_name.clone();
    if matches!(name, "failed" | "aborted" | "canceled") {
        return Err(bad(ID, format!("the job to {printer_name} ended as {name}")));
    }
    Ok(json!({"printer": printer_name, "address": printer.address(), "outcome": name, "pages": doc.pages, "warnings": sent.warnings}))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn print_builds_the_spool_command() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"pages": 4})).unwrap();
        let r = s.execute("file.print", &json!({"printer": "Office", "copies": 2, "pages": "2-3", "dryRun": true})).unwrap();
        assert_eq!(r["pages"], 2);
        let command: Vec<&str> = r["command"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        if craft_print::backend() == Some(craft_print::Backend::Cups) {
            assert_eq!(&command[..5], ["lp", "-d", "Office", "-n", "2"]);
            assert!(command.contains(&"print-scaling=auto-fit"));
            assert!(!command.iter().any(|a| a.starts_with("sides=")));
        } else {
            // Windows: older craft-print previewed a PowerShell command; the in-process XPS path names itself.
            assert!(matches!(command[0], "powershell" | "xps-print"), "{command:?}");
        }
        assert!(r["bytes"].as_u64().unwrap() > 500);
        assert!(s.execute("file.print", &json!({"pages": "9", "dryRun": true})).is_err());
        assert!(s.execute("file.printers", &json!({})).unwrap()["printers"].is_array());
    }

    #[test]
    fn print_submits_the_selected_export_and_reports_refusal() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"pages": 4})).unwrap();
        let params = json!({"printer": "Office (east)", "copies": 2, "pages": "2-3", "marks": true, "bleed": true});
        let expected = {
            let d = &s.doc().unwrap().doc;
            let options = print_options(&params, d.page_count()).unwrap();
            designcraft_pdf::export_pdf(&super::super::interchange::with_pdf_boxes(d), &s.cache, &options).unwrap()
        };
        let r = print_with(&mut s, &params, |pdf, job| {
            // The production export options and full bytes reach the shared spooler boundary.
            assert_eq!(pdf, expected);
            assert!(pdf.starts_with(b"%PDF-"));
            assert_eq!(job.printer.as_deref(), Some("Office (east)"));
            assert_eq!(job.copies, 2);
            assert_eq!(job.duplex, None);
            assert_eq!(job.scaling, craft_print::Scaling::ShrinkToFit);
            assert!(!job.title.is_empty());
            Ok("request id is Office-1".into())
        })
        .unwrap();
        assert_eq!(r["pages"], 2);
        let refusal = print_with(&mut s, &params, |_, _| Err(craft_print::Error::Spool("printer offline".into()))).unwrap_err();
        assert!(refusal.to_string().contains("printer offline"));
        print_with(&mut s, &json!({"dryRun": true}), |_, _| panic!("dry runs must not spool")).unwrap();
        assert!(print_with(&mut s, &json!({"pages": "9"}), |_, _| panic!("invalid ranges must not spool")).is_err());
    }

    /// C: a slide-sized document (16:9, 13.33 x 7.50 in) is printed on the queue's own default paper,
    /// the page shrunk to fit; no paper is demanded of the queue.
    #[test]
    fn a_custom_page_size_uses_the_default_paper_and_shrinks_to_fit() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"width": 960.0, "height": 540.0, "pages": 1})).unwrap();
        let mut seen = None;
        print_with(&mut s, &json!({"printer": "Office"}), |_, job| {
            seen = Some(job.clone());
            Ok("ok".into())
        })
        .unwrap();
        let job = seen.unwrap();
        assert_eq!(job.scaling, craft_print::Scaling::ShrinkToFit);
        assert!(!job.grayscale);
        let r = s.execute("file.print", &json!({"dryRun": true})).unwrap();
        let command = format!("{:?}", r["command"]);
        assert!(!command.contains("media"), "no paper size is asked for: {command}");
        assert!(!command.contains("13.33"), "{command}");
    }

    /// A document with a text hyperlink, a URL button and a form field.
    #[allow(clippy::unwrap_used)]
    fn interactive_session() -> Session {
        let mut s = Session::new();
        s.execute("file.new", &json!({"pages": 2})).unwrap();
        let r = s.execute("frame.create", &json!({"rect": [72, 72, 400, 300], "content": "text", "text": "Visit us online"})).unwrap();
        s.execute("text.select", &json!({"story": r["story"], "anchor": 6, "focus": 8})).unwrap();
        s.execute("hyperlink.create", &json!({"url": "https://example.com"})).unwrap();
        let b = s.execute("frame.create", &json!({"rect": [72, 320, 200, 380]})).unwrap()["id"].clone();
        s.execute("selection.set", &json!({"ids": [b]})).unwrap();
        s.execute("button.set", &json!({"action": "url", "url": "https://example.com/buy"})).unwrap();
        let f = s.execute("frame.create", &json!({"rect": [72, 400, 300, 430]})).unwrap()["id"].clone();
        s.execute("selection.set", &json!({"ids": [f]})).unwrap();
        s.execute("form.set", &json!({"kind": "textField", "name": "who"})).unwrap();
        s
    }

    /// The craft-print Windows path refuses a page that carries annotations (shrink-to-fit cannot
    /// re-frame them; the print-visibility check wants flag 4), so what a print sends has none.
    #[test]
    fn a_document_with_links_buttons_and_fields_prints_with_no_annotations() {
        let mut s = interactive_session();
        let d = &s.doc().unwrap().doc;
        // The normal export keeps the interactive layer (this is what the print used to send).
        let kept = designcraft_pdf::export_pdf(d, &s.cache, &designcraft_pdf::PdfOptions::default()).unwrap();
        let kept = String::from_utf8_lossy(&kept).into_owned();
        assert!(kept.contains("/Annots") && kept.contains("/URI"), "export keeps links");
        assert!(kept.contains("/Widget"), "export keeps form fields");
        let mut sent = None;
        let r = print_with(&mut s, &json!({"printer": "Office"}), |pdf, job| {
            assert_eq!(job.scaling, craft_print::Scaling::ShrinkToFit);
            sent = Some(pdf.to_vec());
            Ok("ok".into())
        })
        .unwrap();
        assert_eq!(r["pages"], 2);
        let sent = sent.expect("the job was sent");
        let text = String::from_utf8_lossy(&sent);
        for bad in ["/Annots", "/Annot", "/URI", "/Link", "/Widget", "/AcroForm", "/Screen"] {
            assert!(!text.contains(bad), "the printed PDF has {bad}");
        }
        // The page art is still there.
        assert_eq!(designcraft_render::pdf_page_count(&sent), Some(2));
        let again = print_document(&mut s, &json!({"pages": "1"})).unwrap();
        assert!(!String::from_utf8_lossy(&again.pdf).contains("/Annots"));
    }

    /// B: a grayscale print sends a PDF whose pixels are all gray (Rec. 709) and tells the driver.
    #[test]
    fn grayscale_prints_gray_pages_and_sets_the_driver_hint() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"pages": 1})).unwrap();
        let id = s.execute("frame.create", &json!({"rect": [50, 50, 350, 350]})).unwrap()["id"].clone();
        s.execute("selection.set", &json!({"ids": [id]})).unwrap();
        s.execute("object.color", &json!({"color": "#ff0000"})).unwrap();
        let coloured = print_document(&mut s, &json!({})).unwrap();
        let gray = print_document(&mut s, &json!({"grayscale": true})).unwrap();
        assert_eq!(gray.pages, coloured.pages);
        let pc = designcraft_render::render_pdf_page(&coloured.pdf, 0, 400).unwrap();
        assert!(pc.data().iter().any(|p| p.r != p.g || p.g != p.b), "the colour print has colour");
        let pm = designcraft_render::render_pdf_page(&gray.pdf, 0, 400).expect("gray page renders");
        assert!(pm.data().iter().all(|p| p.r == p.g && p.g == p.b), "every pixel is gray");
        assert_eq!(designcraft_render::pdf_page_size(&gray.pdf, 0), designcraft_render::pdf_page_size(&coloured.pdf, 0));
        let mut hint = false;
        print_with(&mut s, &json!({"grayscale": true}), |pdf, job| {
            hint = job.grayscale;
            assert!(pdf.starts_with(b"%PDF-"));
            Ok("ok".into())
        })
        .unwrap();
        assert!(hint);
    }

    #[test]
    fn rec709_weights() {
        assert_eq!(designcraft_pdf::rec709_luma(255, 255, 255), 255);
        assert_eq!(designcraft_pdf::rec709_luma(0, 0, 0), 0);
        assert_eq!(designcraft_pdf::rec709_luma(255, 0, 0), 54);
        assert_eq!(designcraft_pdf::rec709_luma(0, 255, 0), 182);
        assert_eq!(designcraft_pdf::rec709_luma(0, 0, 255), 18);
    }

    mod network {
        use std::sync::Arc;

        use designcraft_print::network::wsd::testing::{FakeConfig, FakePrinter, MemTransport};

        use super::*;

        const ADDRESS: &str = "127.0.0.1:5357/printer";

        #[allow(clippy::unwrap_used)] // a test fixture
        fn session() -> Session {
            let mut s = Session::new();
            s.execute("file.new", &json!({"pages": 3})).unwrap();
            s
        }

        fn send(s: &mut Session, printer: &Arc<FakePrinter>, params: Value) -> Result<Value> {
            print_network_with(s, &params, Some(Arc::new(MemTransport(printer.clone()))))
        }

        #[test]
        fn nothing_is_exported_opened_or_sent_without_the_confirmation() {
            let printer = FakePrinter::new(FakeConfig::default());
            let mut s = session();
            for params in [
                json!({"address": ADDRESS}),
                json!({"address": ADDRESS, "user_confirmed": false}),
                json!({"address": ADDRESS, "user_confirmed": "yes"}),
            ] {
                let e = send(&mut s, &printer, params).unwrap_err().to_string();
                assert!(e.contains("user_confirmed"), "{e}");
            }
            assert_eq!(printer.jobs(), 0);
            assert!(printer.log().is_empty(), "the printer was not even asked: {:?}", printer.log());
        }

        #[test]
        fn a_confirmed_print_sends_the_chosen_pages_and_reports_how_it_ended() {
            let printer = FakePrinter::new(FakeConfig::default());
            let mut s = session();
            let r = send(&mut s, &printer, json!({"address": ADDRESS, "user_confirmed": true, "pages": "2-3", "copies": 2})).unwrap();
            assert_eq!(r["outcome"], "completed", "{r}");
            assert_eq!(r["pages"], 2);
            assert!(r["printer"].as_str().is_some_and(|n| !n.is_empty()), "{r}");
            assert_eq!(printer.jobs(), 1);
            let pdf = printer.job(1).unwrap().document.unwrap();
            assert!(pdf.starts_with(b"%PDF-"));
            assert_eq!(designcraft_render::pdf_page_count(&pdf), Some(2), "the chosen range, not the whole document");
        }

        #[test]
        fn the_address_policy_refuses_host_names_public_addresses_and_tls_before_anything_is_exported() {
            let printer = FakePrinter::new(FakeConfig::default());
            let mut s = session();
            for bad in ["printer.local", "http://8.8.8.8/", "https://10.0.0.1/", "ipp://printer.local/ipp", "ipps://10.0.0.1/ipp/print"] {
                assert!(send(&mut s, &printer, json!({"address": bad, "user_confirmed": true})).is_err(), "{bad}");
            }
            assert!(print_network_with(&mut s, &json!({"user_confirmed": true}), None).is_err(), "no address");
            assert_eq!(printer.jobs(), 0);
        }

        #[test]
        fn a_failed_job_is_an_error_not_a_quiet_success() {
            use designcraft_print::network::wsd::testing::Ending;
            let printer = FakePrinter::new(FakeConfig { ending: Ending::Aborts("PrinterStopped"), ..FakeConfig::default() });
            let mut s = session();
            let e = send(&mut s, &printer, json!({"address": ADDRESS, "user_confirmed": true})).unwrap_err().to_string();
            assert!(e.contains("aborted") || e.contains("failed"), "{e}");
        }

        #[test]
        fn it_needs_a_document() {
            let printer = FakePrinter::new(FakeConfig::default());
            let mut s = Session::new();
            assert!(send(&mut s, &printer, json!({"address": ADDRESS, "user_confirmed": true})).is_err());
            assert_eq!(printer.jobs(), 0);
        }
    }
}
