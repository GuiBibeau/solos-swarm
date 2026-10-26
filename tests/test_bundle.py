import copy
import importlib.util
import json
from pathlib import Path
import sys
import struct
import tempfile
import unittest

scripts=Path(__file__).parents[1]/"scripts"
sys.path.insert(0,str(scripts))
import bundle
import qualify


class BundleTests(unittest.TestCase):
    def test_bundle_relocates_exact_bytes_and_alteration_blocks_import(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source=root/"prepared";source.mkdir();header=bytearray(64);header[:6]=b"\x7fELF\x02\x01";struct.pack_into("<HH",header,16,3,62);(source/"runtime").write_bytes(header+b"compiled-fixture-bytes");(source/"runtime").chmod(0o555)
            requirements={"required_artifacts":["runtime"],"executable_artifacts":["runtime"],"features":["production"],"library_requirements":["fixture"],"isolation_profile":"tier-p"}
            archive=root/"candidate.tar";record=bundle.create(source,archive,"revision","x86_64-unknown-linux-gnu",requirements)
            relocated=root/"relocated";bundle.import_bundle(archive,relocated)
            self.assertEqual((relocated/"runtime").read_bytes(),header+b"compiled-fixture-bytes")
            self.assertEqual(record["archive_sha256"],bundle.sha(archive))
            data=archive.read_bytes().replace(b"compiled-fixture-bytes",b"changed-fixture--bytes",1);archive.write_bytes(data)
            with self.assertRaises((ValueError,EOFError)):
                bundle.import_bundle(archive,root/"blocked")
            self.assertFalse((root/"blocked").exists())

    def test_private_credentials_and_external_symlink_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            source=Path(directory)/"source";source.mkdir();(source/"auth.json").write_text('{"key":"canary"}')
            with self.assertRaises(ValueError):bundle.inventory(source)
            (source/"auth.json").unlink();(source/"escape").symlink_to("/etc/passwd")
            with self.assertRaises(ValueError):bundle.inventory(source)

    def test_release_report_cannot_skip_controls_or_borrow_scope(self):
        profile={"agent_count":10,"required_scenarios":sorted(qualify.MANDATORY),"scope":"single-host-preview","min_nodes":1,"allowed_providers":["deepseek"],"max_age_seconds":3600,"limits_digest":"limits","price_policy_digest":"prices","catalog_version":"bounded-goals-v1","verifier_version":"1","isolation_profile":"tier-p"}
        report={**{key:profile[key]for key in ("scope","limits_digest","price_policy_digest","catalog_version","verifier_version","isolation_profile")},"runtime_verdict":"PASS","goal_verdict":"PASS","evidence_verdict":"PASS","cleanup_verdict":"PASS","bundle_sha256":"bundle","participating_agent_ids":[f"a{index}"for index in range(10)],"agent_count":10,"live":True,"provider_available":True,"provider":"deepseek","cleanup_confirmed":True,"held_microusd":0,"independent_node_ids":["host"],"completed_unix":100,"checks":[{"verdict":"PASS"}],"scenarios":[{"id":name,"verdict":"PASS","evidence_refs":["receipt"]}for name in qualify.MANDATORY]}
        report["useful_agent_ids"]=report["participating_agent_ids"][:]
        qualify.validate_report(report,profile,"bundle",100)
        for mutation in ({"useful_agent_ids":[]},{"live":False},{"scenarios":[]},{"scope":"borrowed-goal"},{"held_microusd":1},{"participating_agent_ids":["a"]*10},{"checks":[]}):
            wrong=copy.deepcopy(report);wrong.update(mutation)
            with self.assertRaises(qualify.GateError):qualify.validate_report(wrong,profile,"bundle",100)
        physical=copy.deepcopy(profile);physical["min_nodes"]=3
        labeled=copy.deepcopy(report);labeled["independent_node_ids"]=["node-1","node-2","node-3"]
        with self.assertRaises(qualify.GateError):qualify.validate_report(labeled,physical,"bundle",100)
        physical["independent_node_inventory"]={node:"same-hardware" for node in labeled["independent_node_ids"]}
        labeled["node_hardware_identities"]=physical["independent_node_inventory"]
        with self.assertRaises(qualify.GateError):qualify.validate_report(labeled,physical,"bundle",100)
        physical["independent_node_inventory"]={node:"hardware-"+node for node in labeled["independent_node_ids"]}
        labeled["node_hardware_identities"]=physical["independent_node_inventory"]
        qualify.validate_report(labeled,physical,"bundle",100)


if __name__=="__main__":unittest.main()
