- [x] extract these out to separate funcs then call them in `Commands`
- [x] create more drives(buckets)
- [x] delete drives(buckets)
- upload (putobject) into the drives
- use files (stream)

-- 

## next steps
- restore desired mounts
- winfsp makes buckets appear as windows drive
- storage engine reads write to and from the bucket


- Linux: ~/.config/infinite-storage-engine/config.json
- Windows: %APPDATA%\infinite-storage-engine\config.json


example config: 
```json
{
  "version": 1,
  "defaults": {
    "region": "us-east-1",
    "bucket_prefix": "ise"
  },
  "drives": [
    {
      "id": "uuid",
      "label": "photos",
      "bucket": "ise-photos-a1b2c3",
      "letter": "D",
      "region": "us-east-1",
      "active": true
    }
  ]
}
```

- bucket creation namespace stuff needs to be addressed
    - is adding `ise-` enough?
- i need to figure out howto be able to navigate inside a bucket
