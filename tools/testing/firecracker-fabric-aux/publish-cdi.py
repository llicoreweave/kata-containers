"""Publish only the already-allocated auxiliary endpoints for Kata cold plug."""
import json,socket
from pathlib import Path
assert socket.gethostname()=='s2kxfk44'
assert not Path('/etc/cdi/lli-havock2250-explicit-full-fabric.json').exists()
expected={'1a':('0x15b3','0x1021'),'3c':('0x15b3','0x1021'),'4d':('0x15b3','0x1021'),'5e':('0x15b3','0x1021'),'9c':('0x15b3','0x1021'),'bc':('0x15b3','0x1021'),'cc':('0x15b3','0x1021'),'dc':('0x15b3','0x1021'),'83':('0x10de','0x22a3'),'84':('0x10de','0x22a3'),'85':('0x10de','0x22a3'),'86':('0x10de','0x22a3')}
nodes=[{'path':'/dev/vfio/vfio','permissions':'rw'}]
for bus,identity in sorted(expected.items()):
 p=Path('/sys/bus/pci/devices/0000:'+bus+':00.0')
 assert ((p/'vendor').read_text().strip(),(p/'device').read_text().strip())==identity
 assert (p/'driver').resolve().name=='vfio-pci'
 group=(p/'iommu_group').resolve()
 assert [v.name for v in (group/'devices').iterdir()]==[p.name]
 nodes.append({'path':'/dev/vfio/'+group.name,'permissions':'rw'})
target=Path('/etc/cdi/lli-havock2250-fabric-aux.json')
assert not target.exists()
cdi={'cdiVersion':'0.5.0','kind':'lli.coreweave.com/fabric-aux','devices':[{'name':'s2kxfk44','containerEdits':{'deviceNodes':nodes}}]}
pending=target.with_suffix('.pending');pending.write_text(json.dumps(cdi,indent=2)+'\n');pending.chmod(0o600);pending.replace(target)
print('Published auxiliary CDI: NIC=8, NVSwitch=4, GPU=0')
