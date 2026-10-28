-- The refresh and the expiry sweep find objects by age (docs/remote-cache.md, "GC").
CREATE INDEX objects_uploaded ON objects (uploaded);
