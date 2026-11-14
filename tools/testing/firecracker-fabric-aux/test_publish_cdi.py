"""Exercise the standalone publisher against a temporary sysfs/CDI filesystem."""
import ast
import json
import runpy
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch

PUBLISHER = Path(__file__).with_name('publish-cdi.py')
TREE = ast.parse(PUBLISHER.read_text())
INVENTORY = next(ast.literal_eval(node.value) for node in TREE.body
                 if isinstance(node, ast.Assign)
                 and any(isinstance(target, ast.Name) and target.id == 'expected'
                         for target in node.targets))


class PublisherTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.output = self.root / 'etc/cdi/lli-havock2250-fabric-aux.json'
        self.output.parent.mkdir(parents=True)
        self.groups = []
        driver = self.root / 'sys/bus/pci/drivers/vfio-pci'
        driver.mkdir(parents=True)
        for i, (bus, identity) in enumerate(sorted(INVENTORY.items())):
            bdf = '0000:' + bus + ':00.0'
            device = self.root / 'sys/bus/pci/devices' / bdf
            device.mkdir(parents=True)
            (device / 'vendor').write_text(identity[0])
            (device / 'device').write_text(identity[1])
            (device / 'driver').symlink_to(driver)
            group = self.root / 'sys/kernel/iommu_groups' / str(300 + i)
            (group / 'devices').mkdir(parents=True)
            (group / 'devices' / bdf).symlink_to(device)
            (device / 'iommu_group').symlink_to(group)
            self.groups.append(group)

    def publish(self):
        root = self.root

        def sandbox_path(value):
            path = Path(value)
            return root / path.relative_to('/') if path.is_absolute() else path

        # Replace imports only for the executed publisher. The real pathlib
        # module/class used by the fixture is unchanged, and all writes stay in
        # TemporaryDirectory. No root privileges or real host sysfs are used.
        with patch.dict(sys.modules, {
                'pathlib': types.SimpleNamespace(Path=sandbox_path),
                'socket': types.SimpleNamespace(gethostname=lambda: 's2kxfk44')}):
            runpy.run_path(str(PUBLISHER), run_name='__main__')

    def test_publishes_current_groups_as_valid_json_without_gpu_handles(self):
        self.publish()
        raw = self.output.read_text()
        spec = json.loads(raw)
        self.assertTrue(raw.endswith('\n'))
        self.assertEqual(spec['kind'], 'lli.coreweave.com/fabric-aux')
        nodes = spec['devices'][0]['containerEdits']['deviceNodes']
        expected = {'/dev/vfio/vfio'} | {'/dev/vfio/' + group.name for group in self.groups}
        self.assertEqual({node['path'] for node in nodes}, expected)
        self.assertEqual(len(nodes), 13)
        self.assertFalse(self.output.with_suffix('.pending').exists())

    def test_rejects_shared_iommu_group_before_publishing(self):
        (self.groups[0] / 'devices/0000:9d:00.0').touch()
        with self.assertRaises(AssertionError):
            self.publish()
        self.assertFalse(self.output.exists())

    def test_rejects_gpu_substitution_before_publishing(self):
        bus = sorted(INVENTORY)[0]
        device = self.root / 'sys/bus/pci/devices' / ('0000:' + bus + ':00.0')
        (device / 'vendor').write_text('0x10de')
        (device / 'device').write_text('0x2330')
        with self.assertRaises(AssertionError):
            self.publish()
        self.assertFalse(self.output.exists())

    def test_does_not_overwrite_existing_mapping(self):
        self.output.write_text('existing mapping')
        with self.assertRaises(AssertionError):
            self.publish()
        self.assertEqual(self.output.read_text(), 'existing mapping')


if __name__ == '__main__':
    unittest.main()
