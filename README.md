[![CI](https://github.com/silverbaum/rsmkswap/actions/workflows/ci.yml/badge.svg)](https://github.com/silverbaum/rsmkswap/actions/workflows/ci.yml)
# rsmkswap

Sets up a Linux swap area on a device or file.
Tested on x86_64 GNU/Linux with ext4 and btrfs filesystems.

## Example usage
```
rsmkswap -F -s 81920 swepfile --label SWEPFILE
```
