-- The refresh and the expiry sweep find objects by age (docs/remote-cache.md).
CREATE INDEX objects_uploaded ON objects (uploaded);
