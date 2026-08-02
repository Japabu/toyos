#!/bin/bash
# Write a ToyOS boot image to the USB stick, choosing the stick rather than
# being told which one it is.
#
# The device node is not stable — the same stick has been disk4 and disk6 in
# one day — so a command with a hardcoded /dev/diskN is a command that
# eventually writes to the wrong disk.
#
# Two independent gates stand between this and an internal drive: the
# candidate set is `diskutil list external physical`, and whatever comes out
# of it is re-interrogated for Internal=false and BusProtocol=USB before any
# write. A parse error in the first cannot reach an internal disk, because an
# internal disk fails the second.

set -euo pipefail

IMAGE="${IMAGE:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/bootable-diag.img}"
DRY_RUN=0
FORCE_DISK=""

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run)  DRY_RUN=1; shift ;;
        --disk)     FORCE_DISK="${2#/dev/}"; shift 2 ;;
        --image)    IMAGE="$2"; shift 2 ;;
        -h|--help)
            echo "usage: $0 [--image PATH] [--disk diskN] [--dry-run]"
            echo "  --disk is only needed when more than one USB disk is attached."
            exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

[ -f "$IMAGE" ] || { echo "no image at $IMAGE — run: cargo run -- --diag-boot --build-only" >&2; exit 1; }
[ -s "$IMAGE" ] || { echo "image at $IMAGE is empty" >&2; exit 1; }

prop() { diskutil info -plist "$1" | plutil -extract "$2" raw -o - - 2>/dev/null; }

if [ -n "$FORCE_DISK" ]; then
    CANDIDATES="$FORCE_DISK"
else
    # `external physical` excludes the internal drive and every synthesised
    # APFS container. plutil rather than parsing the human-readable table,
    # which is formatted for reading and not for machines.
    PLIST=$(diskutil list -plist external physical)
    COUNT=$(echo "$PLIST" | plutil -extract WholeDisks raw -o - - 2>/dev/null || echo 0)
    CANDIDATES=""
    for i in $(seq 0 $((COUNT - 1))); do
        CANDIDATES="$CANDIDATES $(echo "$PLIST" | plutil -extract "WholeDisks.$i" raw -o - -)"
    done
    CANDIDATES=$(echo "$CANDIDATES" | tr -s ' ' '\n' | grep . || true)
fi

N=$(echo "$CANDIDATES" | grep -c . || true)
if [ "$N" -eq 0 ]; then
    echo "no external USB disk found. Is the stick plugged in?" >&2
    exit 1
elif [ "$N" -gt 1 ]; then
    echo "more than one external disk is attached — refusing to guess:" >&2
    for d in $CANDIDATES; do
        printf '  /dev/%-8s %s  %s\n' "$d" "$(prop "$d" MediaName)" "$(prop "$d" TotalSize)" >&2
    done
    echo "re-run with --disk diskN" >&2
    exit 1
fi
DISK=$(echo "$CANDIDATES" | tr -d '[:space:]')

# The second gate. Whatever the enumeration above produced, it does not get
# written to unless it says it is an external USB device on its own account.
INTERNAL=$(prop "$DISK" Internal)
BUS=$(prop "$DISK" BusProtocol)
VIRTUAL=$(prop "$DISK" VirtualOrPhysical)
if [ "$INTERNAL" != "false" ] || [ "$BUS" != "USB" ]; then
    echo "/dev/$DISK reports Internal=$INTERNAL BusProtocol=$BUS — refusing to write to it" >&2
    exit 1
fi

SIZE=$(prop "$DISK" TotalSize)
NAME=$(prop "$DISK" MediaName)
IMG_BYTES=$(stat -f %z "$IMAGE")
IMG_TIME=$(stat -f %Sm -t '%Y-%m-%d %H:%M:%S' "$IMAGE")
IMG_SHA=$(shasum -a 256 "$IMAGE" | cut -d' ' -f1)

echo "image   $IMAGE"
echo "        $IMG_BYTES bytes, built $IMG_TIME"
echo "        sha256 $IMG_SHA"
echo "target  /dev/$DISK  \"$NAME\"  $SIZE bytes  ($BUS, ${VIRTUAL:-physical})"
echo

if [ "$DRY_RUN" -eq 1 ]; then
    echo "dry run — nothing written."
    exit 0
fi

printf 'erase /dev/%s and write the image above? [y/N] ' "$DISK"
read -r reply
case "$reply" in
    y|Y) ;;
    *) echo "aborted."; exit 1 ;;
esac

diskutil unmountDisk "/dev/$DISK"
# rdisk is the raw node: no buffer cache, roughly an order of magnitude faster.
# Ctrl-T prints progress; macOS dd has no status=progress.
sudo dd if="$IMAGE" of="/dev/r$DISK" bs=4m
sync
diskutil eject "/dev/$DISK"
echo "done — $IMG_SHA on /dev/$DISK"
