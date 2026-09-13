#!/usr/bin/env python3
"""Synthetic signature fixtures adapted from U60 neighbor v0.7.1-multi.
Datad adds framing, ambiguity, resource and partial-failure regressions.
"""
from pathlib import Path
import json, os, struct, subprocess, sys, tempfile, unittest
HOST=None
def qsh(signature: int, args: list[int]) -> bytes:
    header = bytearray(16)
    header[0] = 0x9d
    header[4] = 0x13 + len(args)
    struct.pack_into('<I', header, 12, signature)
    payload = bytes(header) + b''.join(struct.pack('<I', a & 0xffffffff) for a in args)
    crc = 0xffff
    for byte in payload:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0x8408 if crc & 1 else 0)
    payload += struct.pack('<H', crc ^ 0xffff)
    return b''.join(bytes([0x7d, b ^ 0x20]) if b in (0x7d, 0x7e) else bytes([b]) for b in payload) + b'\x7e'


def snapshot(signature: int, n: int = 12, pci: int = 123, dbm: int = -90) -> bytes:
    a = [0] * n
    a[3:7] = [pci, dbm * 128, 321, -80 * 128]
    return qsh(signature, a)


class SourceTests(unittest.TestCase):
    def parse(self, *captures: bytes) -> dict:
        with tempfile.TemporaryDirectory(prefix='neighbor-fixture-') as name:
            paths = []
            for i, data in enumerate(captures):
                path = Path(name) / f'{i}.qmdl'
                path.write_bytes(data)
                paths.append(str(path))
            result = subprocess.run([str(HOST), '--neighbor-parse', *paths], capture_output=True, check=True)
            return json.loads(result.stdout)

    def test_empty(self):
        result = self.parse(b'')
        self.assertEqual(result['cells'], [])
        self.assertEqual(result['source'], 'qtrace')

    def test_missing_file(self):
        result = subprocess.run([str(HOST), '--neighbor-parse', '/nonexistent-neighbor-fixture.qmdl'], capture_output=True)
        self.assertEqual(result.returncode, 66)

    def test_generic_identity_and_signal(self):
        data = qsh(3640397572, [78, 640000, 123, 0])
        data += snapshot(3657540452, dbm=-90) + snapshot(3657540452, dbm=-80)
        cell = self.parse(data)['cells'][0]
        self.assertEqual((cell['pci'], cell['arfcn'], cell['band']), (123, 640000, 78))
        self.assertEqual(cell['rsrp_dbm'], -85)

    def test_known_firmware_layouts(self):
        layouts = [
            (0xd8facf74, 0xda019f14, 12),
            (0xd8f773e8, 0xda01af24, 11),
            (0xd8fb8ad0, 0xda06aa0c, 12),
            (0xd8f82d98, 0xda06ba1c, 11),
            (0xd8f84514, 0xda06aa0c, 12),
            (0xd8fc5cc0, 0xda0539fc, 12),
            (0xd8fc5cc0, 0xda054a0c, 11),
            (3640387444, 3657934788, 12),
            (3640166840, 3657934788, 12),
            (3640172852, 3657934788, 12),
            (3640387444, 3657937792, 12),
            (3640387444, 3657920232, 12),
        ]
        for identity, signal, n in layouts:
            with self.subTest(identity=hex(identity), signal=hex(signal)):
                result = self.parse(qsh(identity, [78, 640000, 123, 0]) + snapshot(signal, n))
                cell = result['cells'][0]
                self.assertEqual((cell['pci'], cell['arfcn'], cell['rsrp_dbm']), (123, 640000, -90))

    def test_unanchored_measurements(self):
        for signature, n in [(0xda01af24, 11), (0xda06ba1c, 11), (0xda0539fc, 12), (0xda054a0c, 11), (3657934788, 12), (3657937792, 12), (3657920232, 12)]:
            with self.subTest(signature=hex(signature)):
                self.assertEqual(self.parse(snapshot(signature, n))['cells'], [])
        cell = self.parse(snapshot(3657540452))['cells'][0]
        self.assertIsNone(cell['arfcn'])

    def test_no_cross_capture_frequency_backfill(self):
        result = self.parse(qsh(0xd8fb8ad0, [78, 640000, 123, 0]), snapshot(0xda06ba1c, 11))
        self.assertEqual(len(result['cells']), 1)
        self.assertIsNone(result['cells'][0]['rsrp_dbm'])

    def test_b28_anchoring_and_strict_shapes(self):
        identity=qsh(3640387444,[1,424130,676,3])
        measurement=snapshot(3657934788,pci=676,dbm=-108)
        cell=self.parse(identity+measurement)['cells'][0]
        self.assertEqual((cell['arfcn'],cell['band'],cell['rsrp_dbm']),(424130,1,-108))
        cell=self.parse(identity,measurement)['cells'][0]
        self.assertIsNone(cell['rsrp_dbm'])
        for args in ([1,424130,676],[1,424130,676,3,0],[1,0,676,3],[1,424130,1008,3]):
            self.assertEqual(self.parse(qsh(3640387444,args)+measurement)['cells'],[])
        for n in (11,13):
            cell=self.parse(identity+snapshot(3657934788,n,pci=676))['cells'][0]
            self.assertIsNone(cell['rsrp_dbm'])
        other=qsh(3640166840,[78,640000,676,3])
        r=self.parse(identity+measurement+other)
        self.assertEqual(r['ambiguous'],1)
        self.assertTrue(all(c['rsrp_dbm'] is None for c in r['cells']))

    def test_b28_explicit_result_never_rewrites_frequency(self):
        report=qsh(3657646332,[0,628704,587,-121*128,-15*128,0,1])
        cell=self.parse(report)['cells'][0]
        self.assertEqual((cell['pci'],cell['arfcn'],cell['rsrp_dbm']),(587,628704,-121))
        self.assertIsNone(cell['band'])
        wrong_frequency=qsh(3640387444,[1,424130,587,3])
        cells=self.parse(wrong_frequency+report)['cells']
        measured=[c for c in cells if c['rsrp_dbm'] is not None]
        self.assertEqual(len(cells),2)
        self.assertEqual(measured[0]['arfcn'],628704)
        for args in ([0,628704,587,-121*128,-15*128,0], [0,628704,587,-121*128,-15*128,0,1,0],
                     [0,0,587,-121*128,-15*128,0,1], [0,628704,1008,-121*128,-15*128,0,1]):
            self.assertEqual(self.parse(qsh(3657646332,args))['cells'],[])

    def test_b28_unmeasured_floor_is_not_signal(self):
        self.assertEqual(self.parse(qsh(3657646332,[0,628704,587,-19968,-19968,0,1]))['cells'],[])
        for h in (3657934788,3657937792,3657920232):
            cell=self.parse(qsh(3640387444,[78,628704,587,3])+snapshot(h,pci=587,dbm=-156))['cells'][0]
            self.assertIsNone(cell['rsrp_dbm'])
            self.assertEqual(cell['samples'],0)

    def test_other_frequency_is_preserved(self):
        data = qsh(0xd8fb8ad0, [78, 640000, 123, 0]) + snapshot(0xda06ba1c, 11)
        data += qsh(0xd8fb8ad0, [41, 520000, 124, 0]) + snapshot(0xda06ba1c, 11, pci=124, dbm=-70)
        cells = self.parse(data)['cells']
        self.assertEqual({c['arfcn'] for c in cells}, {520000, 640000})

    def test_lte_signal_and_identity_only(self):
        cells = self.parse(qsh(3640546464, [1650, 222, -920, 0]) + qsh(3644228716, [1650, 223]))['cells']
        self.assertEqual((cells[0]['rat'], cells[0]['rsrp_dbm']), ('LTE', -92))
        self.assertIsNone(cells[1]['rsrp_dbm'])

    def test_malformed_and_unknown(self):
        result = self.parse(b'\x01\x7e' + qsh(0x12345678, [1, 2]))
        self.assertEqual(result['malformed'], 1)
        self.assertEqual(result['cells'], [])


    def test_crc_and_incomplete_frame(self):
        good=qsh(3640546464,[1650,222,-920,0])
        corrupt=bytearray(good);corrupt[20]^=1
        for data in (bytes(corrupt),good[:-1],b'x'*65537+b'\x7e'):
            result=self.parse(data);self.assertEqual(result['cells'],[]);self.assertEqual(result['malformed'],1)
        result=self.parse(bytes(corrupt)+good);self.assertEqual(len(result['cells']),1);self.assertTrue(result['partial'])

    def test_no_cross_file_partial_frame(self):
        data=qsh(3640546464,[1650,222,-920,0]);self.assertEqual(self.parse(data[:20],data[20:])['cells'],[])

    def test_ambiguous_frequency_is_not_guessed(self):
        data=qsh(3640397572,[78,640000,123,0])+snapshot(3657540452)+qsh(3640397572,[41,520000,123,0])
        result=self.parse(data);self.assertEqual(result['ambiguous'],1)
        measured=[c for c in result['cells'] if c['rsrp_dbm'] is not None];self.assertEqual(len(measured),1);self.assertIsNone(measured[0]['arfcn'])

    def test_far_identity_is_not_associated(self):
        data=qsh(3640397572,[78,640000,123,0])+qsh(0x12345678,[])*257+snapshot(3657540452)
        measured=[c for c in self.parse(data)['cells'] if c['rsrp_dbm'] is not None];self.assertIsNone(measured[0]['arfcn'])

    def test_same_pci_known_and_unresolved_observations_stay_distinct(self):
        pci=978
        known=qsh(3640397572,[78,627264,pci,0])+qsh(3657646332,[0,627264,pci,-89*128,0,0,1])
        far=qsh(0x12345678,[])*257+snapshot(3657540452,pci=pci,dbm=-90)
        result=self.parse(known+far)
        self.assertEqual(result['malformed'],0)
        self.assertEqual([(c['pci'],c['arfcn']) for c in result['cells']],[(978,627264),(978,None)])

    def test_lte_range_and_zero(self):
        self.assertEqual(self.parse(qsh(3640546464,[0,222,-920,0]))['cells'][0]['arfcn'],0)
        self.assertEqual(self.parse(qsh(3640546464,[0xffffffff,222,-920,0]))['cells'],[])

    def test_bounded_groups_and_latest_signals(self):
        data=b''.join(qsh(3640546464,[1650,i,-920,0]) for i in range(200))
        r=self.parse(data);self.assertEqual(len(r['cells']),128);self.assertTrue(r['partial'])
        data=qsh(3640546464,[1650,222,-1000,0])*100+qsh(3640546464,[1650,222,-800,0])*64
        self.assertEqual(self.parse(data)['cells'][0]['rsrp_dbm'],-80)

    def test_partial_inputs_and_nonregular_limits(self):
        with tempfile.TemporaryDirectory() as name:
            p=Path(name);good=p/'good.qmdl';good.write_bytes(qsh(3640546464,[1650,222,-920,0]))
            r=subprocess.run([str(HOST),'--neighbor-parse',str(good),str(p/'missing')],capture_output=True,timeout=3)
            self.assertEqual(r.returncode,66);self.assertEqual(len(json.loads(r.stdout)['cells']),1);self.assertTrue(json.loads(r.stdout)['partial'])
            fifo=p/'fifo';os.mkfifo(fifo);link=p/'link';link.symlink_to(good);huge=p/'huge'
            with huge.open('wb') as f:f.truncate(32*1024*1024+1)
            for path in (fifo,link,huge):
                r=subprocess.run([str(HOST),'--neighbor-parse',str(path)],capture_output=True,timeout=3);self.assertEqual(r.returncode,66)

if __name__=='__main__':
    HOST=Path(sys.argv.pop(1)).resolve()
    unittest.main(verbosity=2)
