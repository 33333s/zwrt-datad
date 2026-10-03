#!/usr/bin/env python3
"""uci fixture for the SMS test: only the send-encryption flag exists, and only
on the firmware being simulated as MU5252-like (`encrypted_send`)."""
import json
import os
from pathlib import Path
import sys

config = json.loads((Path(os.environ['SMS_FIXTURE']) / 'config.json').read_text())
if sys.argv[1:] == ['-q', 'get', 'zwrt_wms.config.sms_no_need_encryption_flag'] and config.get('encrypted_send', True):
    print('0')
    sys.exit(0)
sys.exit(1)
