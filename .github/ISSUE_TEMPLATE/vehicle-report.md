---
name: New vehicle report
about: Tell us how OrangeCar behaves with your car
title: "[vehicle] <make> <model> <year>"
labels: vehicle-report
---

**Car**: make, model, year, market
**Head unit**: name/version if known (CarPlay settings or service menu)
**Board**: (e.g. Orange Pi 5 Plus 16 GB, Armbian version, kernel)
**OrangeCar commit**:

**How far did it get?** (tick the last step reached)
- [ ] Board enumerated as an iPhone (car shows a CarPlay prompt or icon)
- [ ] Car sent the role-switch request (`0x51`)
- [ ] Car enumerated as a USB device after the switch (VID:PID: ____)
- [ ] iAP2 link up, identification accepted
- [ ] CDC-NCM link-local address up
- [ ] RTSP SETUP / GET /info answered (display size: ____)
- [ ] Video shown on the car screen
- [ ] Touch works
- [ ] Audio works
- [ ] Session stays up for a whole drive

**Logs**: attach the run log from `/var/lib/rascar-debug/` (remove VIN, serials and anything personal).
