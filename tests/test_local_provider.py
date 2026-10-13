import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("local_provider", Path(__file__).parents[1] / "scripts/local_provider.py")
provider = importlib.util.module_from_spec(spec)
spec.loader.exec_module(provider)


class LocalProviderIntegrity(unittest.TestCase):
    def test_every_extracted_library_and_extra_file_is_checked_before_execution(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "tree"
            root.mkdir()
            archive_path = Path(temporary) / "pinned.tar.gz"
            with tarfile.open(archive_path, "w:gz") as archive:
                for name, body in {"release/llama-server": b"binary", "release/libggml.so.1": b"library"}.items():
                    member = tarfile.TarInfo(name)
                    member.size = len(body)
                    member.mode = 0o755
                    archive.addfile(member, io.BytesIO(body))
                link = tarfile.TarInfo("release/libggml.so")
                link.type = tarfile.SYMTYPE
                link.linkname = "libggml.so.1"
                archive.addfile(link)
            with tarfile.open(archive_path) as archive:
                archive.extractall(root, filter="data")
            provider.verify_extraction(archive_path, root)
            library = root / "release/libggml.so.1"
            library.write_bytes(b"tamper!")
            with self.assertRaises(ValueError):
                provider.verify_extraction(archive_path, root)
            library.write_bytes(b"library")
            (root / "release/unpinned.so").write_bytes(b"new")
            with self.assertRaises(ValueError):
                provider.verify_extraction(archive_path, root)
            (root / "release/unpinned.so").unlink()
            (root / "release/libggml.so").unlink()
            (root / "release/libggml.so").symlink_to("/outside/libggml.so.1")
            with self.assertRaises(ValueError):
                provider.verify_extraction(archive_path, root)
