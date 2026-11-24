use vmlab

fn main(lab: Lab) {
    let vm = lab.vm("temple").expect("temple")
    vm.type_text_paced("VmlabAgentInstall;\nFileWrite(\"~/F7DONE.TXT\",\"ok\",2);\n", 40)
        .expect("reinstall at the TempleOS shell")
}
