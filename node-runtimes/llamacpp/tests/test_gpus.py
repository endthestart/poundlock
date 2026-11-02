import unittest

from poundlock_llamacpp.gpus import device_arguments, largest_first, parse_devices

LISTING = """ggml_metal: device 0: AMD Radeon Pro 580X (peer group 0, not bridged)
Available devices:
  MTL0: AMD Radeon Pro 580X (dev p0/v0) (8192 MiB, 8191 MiB free)
  MTL1: AMD Radeon Pro W5700X (dev p0/v1) (16368 MiB, 16367 MiB free)
  BLAS: Accelerate (0 MiB, 0 MiB free)
"""


class GpuTests(unittest.TestCase):
    def test_lists_the_gpus_and_leaves_out_host_backends(self) -> None:
        devices = parse_devices(LISTING)
        self.assertEqual(["MTL0", "MTL1"], [device.name for device in devices])
        self.assertEqual("AMD Radeon Pro W5700X (dev p0/v1)", devices[1].description)
        self.assertEqual(16368, devices[1].total_mib)

    def test_orders_the_largest_first_and_keeps_listing_order_for_equal_sizes(self) -> None:
        listing = (
            "  Vulkan0: A (8192 MiB, 8000 MiB free)\n"
            "  Vulkan1: B (24576 MiB, 24000 MiB free)\n"
            "  Vulkan2: C (8192 MiB, 8000 MiB free)\n"
        )
        ordered = largest_first(parse_devices(listing))
        self.assertEqual(("--device", "Vulkan1,Vulkan0,Vulkan2"), device_arguments(ordered))
        self.assertEqual((), device_arguments([]))


if __name__ == "__main__":
    unittest.main()
