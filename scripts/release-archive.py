"""Extract only a flat, bounded release archive after its Actions digest was verified."""
import pathlib
import re
import sys
import zipfile


def extract(archive, destination):
    destination = pathlib.Path(destination).resolve()
    if destination.exists():
        raise ValueError("release extraction directory must not exist")
    with zipfile.ZipFile(archive) as source:
        entries = source.infolist()
        names = [entry.filename for entry in entries]
        allowed = re.compile(r"(?:Nitely_[0-9A-Za-z.-]+_x64-setup\.exe(?:\.sig)?|nitely_[0-9A-Za-z.-]+_x86_64-pc-windows-msvc\.exe|candidate\.json|SHA256SUMS\.txt)")
        if len(names) != len(set(names)) or not 4 <= len(names) <= 5:
            raise ValueError("invalid release archive inventory")
        if any(not allowed.fullmatch(name) for name in names):
            raise ValueError("unexpected path in release archive")
        if sum(entry.file_size for entry in entries) > 1024 * 1024 * 1024:
            raise ValueError("release archive exceeds one GiB")
        destination.mkdir()
        for entry in entries:
            # Write bytes explicitly: never honor archive permissions, links or paths.
            with source.open(entry) as src, (destination / entry.filename).open("xb") as dst:
                while chunk := src.read(1024 * 1024):
                    dst.write(chunk)


if __name__ == "__main__":
    extract(*sys.argv[1:])
