// Event handler for host.disk_low (PRD §8.1).

use vmlab

fn handle(event: Event, lab: Lab) {
    lab.log("DISK LOW: " + event.data)
}
