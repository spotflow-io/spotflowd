"""Run inside an isolated Poky build directory (see test_integration.py)."""

import os
import hashlib
import logging
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import tomllib

import bb.data
import bb.fetch2
import bb.tinfoil


ROOT = Path(__file__).resolve().parents[2]
RECIPE_DIR = ROOT / "yocto/meta-spotflow/recipes-connectivity/spotflowd"
RECIPE = next(RECIPE_DIR.glob("spotflowd_*.bb"))
FILES = RECIPE_DIR / "files"


with bb.tinfoil.Tinfoil() as tinfoil:
    tinfoil.prepare(config_only=True)
    for init in ("systemd", "sysvinit", "systemd sysvinit"):
        for features in ("", "crashdump", "journald", "journald crashdump"):
            if init == "sysvinit" and "journald" in features:
                continue
            config = bb.data.createCopy(tinfoil.config_data)
            distro = set(config.getVar("DISTRO_FEATURES").split()) - {"systemd", "sysvinit"}
            config.setVar("DISTRO_FEATURES", " ".join(sorted(distro)) + " " + init)
            config.setVar("PACKAGECONFIG:pn-spotflowd", features)
            data = tinfoil.parse_recipe_file(str(RECIPE), config_data=config)
            journald = "journald" in features
            crashdump = "crashdump" in features
            user = "spotflow" if journald and not crashdump else "root"
            assert data.getVar("SPOTFLOWD_USER") == user
            assert "--frozen" in data.getVar("CARGO_BUILD_FLAGS")
            assert data.getVar("CARGO_PROFILE_RELEASE_STRIP") == "false"
            assert data.getVar("CARGO_PROFILE_RELEASE_PANIC") == data.getVar("RUST_PANIC_STRATEGY")
            assert data.getVarFlag("CARGO_PROFILE_RELEASE_PANIC", "export")
            assert ("--features journald" in data.getVar("CARGO_BUILD_FLAGS")) == journald
            assert "pkgconfig-native" in data.getVar("DEPENDS").split()
            assert data.getVar("LICENSE") == "BUSL-1.1"
            assert (Path(data.getVar("COMMON_LICENSE_DIR")) / "BUSL-1.1").is_file()
            deps = data.getVar("RDEPENDS:spotflowd").split()
            assert "ca-certificates" in deps
            assert ("busybox-syslog" in deps) == (not journald)
            assert ("dpkg-start-stop" in deps) == ("sysvinit" in init)

            with tempfile.TemporaryDirectory() as temp:
                work = Path(temp)
                data.setVar("WORKDIR", str(work))
                image = work / "image"
                data.setVar("D", str(image))
                unpack = Path(data.getVar("SPOTFLOWD_UNPACKDIR"))
                unpack.mkdir(parents=True, exist_ok=True)
                if init == "systemd" and features == "":
                    # Stage the actual working-tree inputs with BitBake's file
                    # fetcher. No .git/target tree or compatibility patch is needed.
                    data.setVar("DL_DIR", str(work / "downloads"))
                    data.setVar("BB_NO_NETWORK", "1")
                    local_sources = [uri for uri in data.getVar("SRC_URI").split() if uri.startswith("file://")]
                    fetcher = bb.fetch2.Fetch(local_sources, data)
                    fetcher.download()
                    fetcher.unpack(str(unpack))
                    source = Path(data.getVar("S"))
                    for name in ("Cargo.toml", "Cargo.lock", "LICENSE.MD"):
                        assert (source / name).read_bytes() == (ROOT / name).read_bytes()
                    for original in (ROOT / "src").rglob("*.rs"):
                        assert (source / original.relative_to(ROOT)).read_bytes() == original.read_bytes()
                    assert not (source / ".git").exists()
                    assert not (source / "target").exists()
                    checksum = hashlib.md5((source / "LICENSE.MD").read_bytes()).hexdigest()
                    assert f"file://LICENSE.MD;md5={checksum}" == data.getVar("LIC_FILES_CHKSUM")
                for source in FILES.glob("spotflowd.*"):
                    shutil.copy2(source, unpack / source.name)
                binary = Path(data.getVar("B")) / "target" / data.getVar("CARGO_TARGET_SUBDIR") / "spotflowd"
                binary.parent.mkdir(parents=True)
                binary.write_text("#!/bin/sh\nexit 0\n")
                tools = work / "tools"
                tools.mkdir()
                chown_log = work / "chown.log"
                chown = tools / "chown"
                chown.write_text(f'#!/bin/sh\nprintf "%s\\n" "$*" >> "{chown_log}"\n')
                chown.chmod(0o755)
                env = os.environ.copy()
                env["PATH"] = str(tools) + os.pathsep + shutil.which("sh").rsplit("/", 1)[0] + os.pathsep + env["PATH"]
                subprocess.run(["sh", "-ec", data.getVar("do_install")], env=env, check=True)

                cfg_path = image / "etc/spotflow/spotflowd.toml"
                cfg = tomllib.loads(cfg_path.read_text())
                assert cfg["logs"]["journald"] == journald
                assert cfg["logs"]["syslog"] != journald
                assert cfg["crashdump"]["enabled"] == crashdump
                assert cfg["storage"]["state_dir"] == "/var/lib/spotflow"
                assert stat.S_IMODE(cfg_path.stat().st_mode) == 0o640
                assert stat.S_IMODE((image / "var/lib/spotflow").stat().st_mode) == 0o750
                assert f"root:{data.getVar('SPOTFLOWD_GROUP')}" in chown_log.read_text()
                assert not (image / "run").exists()
                assert not (image / "var/run").exists()
                assert (image / "etc/init.d/spotflowd").exists() == ("sysvinit" in init)
                unit = image / data.getVar("systemd_system_unitdir").lstrip("/") / "spotflowd.service"
                assert unit.exists() == ("systemd" in init)
                if unit.exists():
                    content = unit.read_text()
                    assert "@" not in content
                    assert f"User={user}\n" in content
                    assert " adm" not in content
                    assert "network-online" not in content
                    assert "ExecCondition=/usr/sbin/spotflowd config-check --config /etc/spotflow/spotflowd.toml" in content
                    assert "ExecStart=/usr/sbin/spotflowd run --config /etc/spotflow/spotflowd.toml" in content
                    assert ("ReadWritePaths=-/sys/fs/pstore" in content) == crashdump
                    if shutil.which("systemd-analyze"):
                        # Required default targets in an otherwise empty test root.
                        for name in ("sysinit", "basic", "multi-user", "shutdown"):
                            unit.with_name(f"{name}.target").write_text("[Unit]\nDefaultDependencies=no\n")
                        subprocess.run(["systemd-analyze", f"--root={image}", "--man=no", "verify", "spotflowd.service"], check=True)
                print(f"validated {init}: PACKAGECONFIG={features!r}")

    # The class-selected default must agree with the runtime log source.
    assert tinfoil.parse_recipe_file(str(RECIPE)).getVar("PACKAGECONFIG") == "journald"
    config = bb.data.createCopy(tinfoil.config_data)
    config.setVar("DISTRO_FEATURES", "sysvinit")
    assert tinfoil.parse_recipe_file(str(RECIPE), config_data=config).getVar("PACKAGECONFIG") == ""

    # A distro that builds its Rust target with abort must be followed too;
    # the recipe must not hardcode unwind independently of the sysroot.
    config = bb.data.createCopy(tinfoil.config_data)
    config.setVar("RUST_PANIC_STRATEGY", "abort")
    assert tinfoil.parse_recipe_file(str(RECIPE), config_data=config).getVar("CARGO_PROFILE_RELEASE_PANIC") == "abort"

    # Discover the production runtime suite with the real OEQA loader. Its SSH
    # test has a transitive dependency on ping even with slirp/localhost.
    sys.path.insert(0, str(Path(data.getVar("COREBASE")) / "meta/lib"))
    from oeqa.runtime.context import OERuntimeTestContext

    suites = re.search(r'^TEST_SUITES = "([^"]+)"', (ROOT / "yocto/ci-image.sh").read_text(), re.MULTILINE)[1]
    runtime = OERuntimeTestContext({}, logging.getLogger("runtime-discovery"), None, {}, "/tmp")
    runtime.loadTests([
        str(Path(data.getVar("COREBASE")) / "meta/lib/oeqa/runtime/cases"),
        str(ROOT / "yocto/meta-spotflow/lib/oeqa/runtime/cases"),
    ], modules=suites.split())
    assert "spotflowd.SpotflowdTest.test_offline_collection_and_restart" in runtime._registry["cases"]
    print("validated runtime suite dependencies")
