import importlib.util
import pathlib
import tempfile
import unittest
import zipfile

spec = importlib.util.spec_from_file_location("release_archive", pathlib.Path(__file__).with_name("release-archive.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class ArchiveTest(unittest.TestCase):
    def archive(self, directory, names):
        archive = pathlib.Path(directory) / "candidate.zip"
        with zipfile.ZipFile(archive, "w") as output:
            for name in names:
                output.writestr(name, b"tested")
        return archive

    def test_flat_artifacts_only(self):
        names = ["Nitely_1.2.3_x64-setup.exe", "nitely_1.2.3_x86_64-pc-windows-msvc.exe", "candidate.json", "SHA256SUMS.txt"]
        with tempfile.TemporaryDirectory() as directory:
            archive = self.archive(directory, names)
            destination = pathlib.Path(directory) / "out"
            module.extract(archive, destination)
            self.assertEqual(sorted(names), sorted(path.name for path in destination.iterdir()))
        for bad in ["../escape.exe", "/absolute.exe", "folder/setup.exe", "candidate.json"]:
            with self.subTest(bad=bad), tempfile.TemporaryDirectory() as directory:
                archive = self.archive(directory, names + [bad])
                destination = pathlib.Path(directory) / "out"
                with self.assertRaises(ValueError):
                    module.extract(archive, destination)
                self.assertFalse(destination.exists())


if __name__ == "__main__":
    unittest.main()
