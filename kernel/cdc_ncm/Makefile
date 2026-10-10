# Out-of-tree build of cdc_ncm with a random-MAC fallback (RasCar / CarPlay head units).
obj-m := cdc_ncm.o
KDIR ?= /lib/modules/$(shell uname -r)/build

all:
	$(MAKE) -C $(KDIR) M=$(CURDIR) modules

clean:
	$(MAKE) -C $(KDIR) M=$(CURDIR) clean
