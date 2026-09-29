// Screen from wscript: a screenshot on disk, OCR of the console, and a wait
// for text that is on it.
use vmlab

fn run(lab: Lab) -> Result[unit, string] {
    let vm = lab.vm("vm01")?
    let path = vm.screenshot("shots/vm01.png")?
    lab.log("e2e-shot " + path)
    let m = vm.wait_for_text("(?i)vision", 60)?
    lab.log("e2e-wait-text " + m.text)
    let text = vm.ocr()?
    lab.log("e2e-ocr-begin")
    lab.log(text)
    lab.log("e2e-ocr-end")
    Ok(())
}

fn main(lab: Lab) {
    match run(lab) {
        Ok(u) => u,
        Err(e) => {
            let failed: Result[unit, string] = Err(e)
            failed.expect("vision failed: " + e)
        },
    }
}
