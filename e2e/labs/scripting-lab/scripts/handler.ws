// Every bound event lands here; the line is what the scenario looks for in
// the lab daemon's log.
use vmlab

fn handle(event: Event, lab: Lab) {
    lab.log("e2e-handler " + event.name + " vm=" + event.vm)
}
