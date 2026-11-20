#!/usr/bin/env python3
"""Reproduce display/name tables from the supplied UFIPanel Kotlin export.

Usage: python scripts/import-web-reference-metadata.py reference.zip [--check]
No captures, credentials or host state are copied.
"""
import argparse
import json
from pathlib import Path
import re
import zipfile

args = argparse.ArgumentParser()
args.add_argument('bundle', type=Path)
args.add_argument('--check', action='store_true')
options = args.parse_args()
output = Path(__file__).resolve().parents[1] / 'web'
quoted = r'"(?:[^"\\]|\\.)*"'

with zipfile.ZipFile(options.bundle) as archive:
    def source(name):
        paths = [p for p in archive.namelist() if p.endswith('/data/zwrt/signal/' + name + '.kt')]
        if len(paths) != 1:
            raise ValueError(f'Expected exactly one source: {name}')
        return archive.read(paths[0]).decode('utf-8')

    def body(text, name, kind='mapOf'):
        match = re.search(r'\bval\s+' + name + r'(?:\s*:\s*[^=]+)?\s*=\s*' + kind + r'\((.*?)\n    \)', text, re.S)
        if not match:
            raise ValueError(f'Missing table: {name}')
        return match[1]

    def table(text, name, numeric=False):
        pattern = (r'(0x[0-9a-fA-F]+|\d+)' if numeric else '(' + quoted + ')') + r'\s+to\s+(' + quoted + ')'
        return {str(int(k, 0)) if numeric else json.loads(k): json.loads(v) for k, v in re.findall(pattern, body(text, name))}

    nas, signal, engineering = source('NasDecoder'), source('SignalDecoder'), source('CellEngineering')
    generated = {
        'nas-names': {k: table(nas, k, k != 'PLMN') for k in ['MM_NAMES', 'SM_NAMES', 'PLMN']},
        'signal-names': {**table(signal, 'CODE_NAMES', True), **table(signal, 'TEMPLATE_FAMILIES', True)},
        'rrc-labels': {'excluded': [json.loads(q) for q in re.findall(quoted, body(engineering, 'excluded', 'setOf'))],
                       **{k: table(engineering, k) for k in ['titles', 'labels']}},
    }
    for name, data in generated.items():
        path = output / (name + '.json')
        if options.check:
            if json.loads(path.read_text(encoding='utf-8')) != data:
                raise ValueError(f'Generated table differs: {name}')
        else:
            path.write_text(json.dumps(data, ensure_ascii=False, separators=(',', ':')) + '\n', encoding='utf-8')
        print(f'{name}: verified' if options.check else f'{name}: imported')
