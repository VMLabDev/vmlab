// The inputs a recording should show: text typed at the console, a pointer
// move and a click. The text is printed so OCR has something to find.
use vmlab

fn main(lab: Lab) {
    let vm = lab.vm("vm01").expect("vm01")
    vm.type_text("VMLAB RECORD PROBE").expect("type")
    lab.log("e2e-typed")
    vm.mouse_move(300, 200).expect("move")
    vm.mouse_click("left").expect("click")
    lab.log("e2e-clicked")
}
