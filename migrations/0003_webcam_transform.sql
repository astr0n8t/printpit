-- Camera orientation chosen by an admin: NULL = whatever Moonraker's webcam config says,
-- otherwise one of webcam::Transform::CHOICES ('none', 'flip_h', 'flip_v', 'rot_90', …).
ALTER TABLE printers ADD COLUMN webcam_transform TEXT;
