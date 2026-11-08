HopOS for the Mac mini (Apple silicon)
======================================

This turns a Mac mini into a HopOS node. macOS stays on the machine, smaller;
HopOS becomes what the Mac starts when you power it on.

What you need
-------------
* A Mac mini with Apple silicon, a keyboard, a mouse and a screen (Recovery is
  a graphical environment; this is the only step that needs one).
* This image, written to a USB drive: use hop-imager, or
  `gunzip -c hopos-apple-headless.img.gz | sudo dd of=/dev/rdiskN bs=4m`.
  Do not format the drive yourself: the image brings its own FAT filesystem,
  which is what Recovery can read.
* Ten minutes, and the willingness to shrink macOS.

Step 0: allow it, once
----------------------
From Recovery: Startup Security Utility -> Security Policy -> Reduced Security,
and tick both boxes (kernel extensions, and remote management). Apple calls the
result Permissive Security; without it the Mac will not start anything but
Apple's own kernel.

Step 1: get into Recovery
-------------------------
Shut the Mac down. Then press and HOLD the power button (do not tap it) until
"Loading startup options" appears. Choose Options -> Continue, then open
Utilities -> Terminal from the menu bar.

Step 2: run the installer
-------------------------
You are reading this from the USB drive you wrote this image to, so it is
already plugged in and it mounts as HOPOS. In the Terminal:

    sh /Volumes/HOPOS/install.sh          # shows the plan, changes nothing
    sh /Volumes/HOPOS/install.sh go       # does it

The first command is a dry run: it shows the disk, how much macOS is using, how
much it will keep and how much HopOS gets. The second one asks for confirmation
before it shrinks anything.

How much does macOS keep? Whatever it is using plus 20 GB, and never less than
60. Override either number if you want a different split:

    MARGIN_GB=40 FLOOR_GB=120 sh /Volumes/HOPOS/install.sh go

Step 3: restart
---------------
That is it. The Mac now comes up as a HopOS node.

What to expect
--------------
No screen output. The display firmware on this hardware does not come up for
us, so a Mac mini node is headless; that is a property of the machine, not a
fault. The node tells you it is alive over the network: it takes a DHCP lease,
names itself apple-<tail of its MAC> unless the config says otherwise, and
serves its welcome page on port 80. Its console is on TCP port 5555
(`nc <node> 5555`).

The config (hopos.cfg) lives inside the image, in its config window: there is
no filesystem on this board that HopOS can read. To change it, put another one
in with `hop image /Volumes/HOPOS/hopos-apple.img --config my-node.cfg` (the
hop command, github.com/hop-os/hop) and run the installer again.

Going back to macOS
-------------------
    sh /Volumes/HOPOS/install.sh revert

Recovery boots from its own partition and is never affected by what we install,
so this way back always exists: hold the power button and you are there.

Updating HopOS
--------------
Two ways, and which one you use depends on whether the node has to keep
running.

The safe one, always available: write a newer image to this drive and run
install.sh go again. A full image, a reboot, nothing clever.

The live one, for a node you would rather not interrupt: HopOS can replace its
own kernel while the apps on it keep running. Take hopos-apple-headless.flip
from the same release (or build one: `sh image/flip-bundle.sh apple`, which
prints the bundle's sha256), put it on a webserver the node can reach, and ask
the node for it:

    curl -X POST http://<node>:8080/flip \
      -d '{"url":"http://<host>/hopos-apple-headless.flip","sha256":"<64 hex>"}'

The node fetches it, checks the sum against the one you gave, and jumps into
the new kernel; the apps, their network connections and Hop's own state
survive the swap. Watch it happen on the console (port 5555).

That request goes through Hop's API and is signed like any other: asking for a
kernel needs the same key as dispatching a job, because a kernel from the
network is code with every right on this machine. Nothing is ever fetched on a
schedule or on someone else's say-so: a node only replaces itself when you ask
it to, and if the new kernel does not come up the watchdog reboots the machine
straight back into the image on the internal disk.

What is on this drive
---------------------
install.sh        the installer (dry run by default)
hopos-apple.img   HopOS itself, with Hop and the config baked in
README.txt        this file
