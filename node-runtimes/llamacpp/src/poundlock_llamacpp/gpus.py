"""The GPUs a server uses, found each time it starts.

A machine's device order is not fixed: after a reboot that moves the displays to another GPU,
Metal can list that GPU first. A profile with `server.gpu_order` therefore names no device; the
runtime asks the server binary which GPUs it sees and passes them, ordered, as `--device`, so
`--tensor-split` keeps meaning the same GPUs.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Dict, List, Sequence, Tuple

GPU_ORDERS = frozenset({"largest_first"})

# "  MTL0: AMD Radeon Pro 580X (dev p0/v0) (8192 MiB, 8191 MiB free)"
DEVICE_LINE = re.compile(r"^\s+([A-Za-z]+[0-9]+): (.+?) \((\d+) MiB, (\d+) MiB free\)\s*$")


@dataclass(frozen=True)
class Device:
    name: str
    description: str
    total_mib: int

    def evidence(self) -> Dict[str, object]:
        return {"device": self.name, "description": self.description, "total_mib": self.total_mib}


def parse_devices(output: str) -> List[Device]:
    """The GPUs in `llama-server --list-devices` output. Host backends such as BLAS report no
    memory and are left out."""
    devices: List[Device] = []
    for line in output.splitlines():
        match = DEVICE_LINE.match(line)
        if match and int(match.group(3)) > 0:
            devices.append(Device(match.group(1), match.group(2), int(match.group(3))))
    return devices


def largest_first(devices: Sequence[Device]) -> List[Device]:
    """Most memory first; GPUs of equal size keep the order the server listed them in."""
    return sorted(devices, key=lambda device: -device.total_mib)


def device_arguments(devices: Sequence[Device]) -> Tuple[str, ...]:
    return ("--device", ",".join(device.name for device in devices)) if devices else ()
