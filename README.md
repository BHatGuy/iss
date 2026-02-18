# Immich Share Sync - iss


A small tool to sync multiple shared [Immich](https://immich.app/) albums. I didn't test this very thoroughly yet, so feel free to report bugs or propose improvements :)

## Configuration

The configuration is done via a toml file, which has to be provided via the -c/--config argument. You can test your config with -d/--dry-run

Example configuration:
``` toml
[Some_Album]
shared_link = "https://immich.example.org/share/this_key_will_be_longer"
# This album will receive all assets from Another_Shard_Album and Third_Album, that are missing form this one
sync_with = ["Another_Shard_Album", "Third_Album"]

[Another_Shard_Album]
shared_link = "https://immich.foo.org/share/this_key_will_be_longer"
# This album will receive all assets from Some_Album, that are missing form this one
sync_with = ["Some_Album"]

[Third_Album]
shared_link = "https://immich.bar.org/share/this_key_will_be_longer"
# There will be no uploads to this album
sync_with = []
```

## Caveats

Currently if there are multiple albums, that are connected, but not fully connected, multiple runs might be required for until all assets are synced. This is due to the fact, that every connection is synced separately.

