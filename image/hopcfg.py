#!/usr/bin/env python3
"""hopcfg: hopos.cfg in het config-venster van een kern-image zetten of lezen.

    python3 image/hopcfg.py set  <bestand> <cfg>...  de config in het venster
    python3 image/hopcfg.py show <bestand>           de config uit het venster

Meer dan één <cfg>: aaneengeplakt in die volgorde (de config van een smaak
uit image/cfg, headless.cfg of headfull.cfg, met een eigen bestand
erachter); de kern leest de laatste waarde van een sleutel.

<bestand> is alles wat de kern draagt: de kern-ELF, kernel8.img, het
arm64-Image van de Radxa, BOOTAA64.EFI, het bootobject van de M4,
monitor.bin van de LicheeRV, een flipbundel. De Go-generatie had dit als
image/hopcfg (tag v2.2.8); het formaat is dat van board/src/cfgwin.rs:

    #HOPCFG1 window=16384 len=0000000432
    hopos.node=hop-1
    ...
    ################################################################ (padding)

Het script zoekt de kopregel op elke byte (zoals Go), eist precies één
venster, en schrijft exact de venstermaat terug: het bestand verandert van
inhoud, niet van lengte. Een config die zelf al een venster is, wordt eerst
gestript (Go: idempotent). Te lang, een NUL of geen UTF-8: weigeren, want de
kern zou hem negeren. Op een kaart of een stick, of met de FIP-checksums van
de LicheeRV: `hop image` in de hop-repo.
"""
import re
import sys

MAGIC = b"#HOPCFG1 window="
HEAD = re.compile(rb"#HOPCFG1 window=(\d{1,10}) len=(\d{10})\n")


def head(b, at):
    """(maat, kopregel, lengte) van het venster op `at`, of None."""
    m = HEAD.match(b, at)
    if not m:
        return None
    size, n = int(m.group(1)), int(m.group(2))
    h = m.end() - at
    if size % 512 or h + n > size:
        return None
    return size, h, n


def windows(b):
    out, at = [], b.find(MAGIC)
    while at >= 0:
        if head(b, at):
            out.append(at)
        at = b.find(MAGIC, at + 1)
    return out


def one(b, path):
    w = windows(b)
    if len(w) != 1:
        sys.exit(f"hopcfg: {len(w)} config windows in {path} (at {[hex(x) for x in w]}), expected 1")
    return w[0]


def window(text, size):
    """Exact `size` bytes: kopregel, config, '#'-regels (Go's makeWindow)."""
    if text.startswith(MAGIC):
        h = head(text, 0)
        if h:
            text = text[h[1]:h[1] + h[2]]
    if b"\0" in text:
        sys.exit("hopcfg: the config contains a NUL byte")
    try:
        text.decode("utf-8")
    except UnicodeDecodeError:
        sys.exit("hopcfg: the config is not UTF-8")
    if text and not text.endswith(b"\n"):
        text += b"\n"
    w = MAGIC + b"%d len=%010d\n" % (size, len(text)) + text
    if len(w) > size:
        sys.exit(f"hopcfg: a config of {len(text)} bytes does not fit the {size}-byte window")
    while len(w) < size:
        n = min(65, size - len(w))
        w += b"#" * (n - 1) + b"\n"
    return w


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "show":
        b = open(sys.argv[2], "rb").read()
        at = one(b, sys.argv[2])
        size, h, n = head(b, at)
        print(f"hopcfg: window of {size} bytes at {at:#x}, {n} bytes of config", file=sys.stderr)
        sys.stdout.buffer.write(b[at + h:at + h + n])
    elif len(sys.argv) >= 4 and sys.argv[1] == "set":
        path, cfgs = sys.argv[2], sys.argv[3:]
        b = bytearray(open(path, "rb").read())
        at = one(b, path)
        size = head(b, at)[0]
        text = b""
        for c in cfgs:
            layer = open(c, "rb").read()
            if layer and not layer.endswith(b"\n"):
                layer += b"\n"
            text += layer
        w = window(text, size)
        cfg = " + ".join(cfgs)
        b[at:at + size] = w
        open(path, "r+b").write(b)
        n = head(w, 0)[2]
        print(f"hopcfg: {cfg} ({n} bytes) in the window at {at:#x} of {path}", file=sys.stderr)
    else:
        sys.exit(__doc__.split("\n\n")[1])


if __name__ == "__main__":
    main()
