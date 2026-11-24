use vmlab

fn main(lab: Lab) {
    let vm = lab.vm("temple").expect("temple")
    vm.type_text("n").expect("decline the TempleOS tour")
}
