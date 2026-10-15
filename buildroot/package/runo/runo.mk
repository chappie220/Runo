################################################################################
#
# runo
#
################################################################################

# The binary is built outside Buildroot (see Config.in); this only
# installs it, with the files that make the image a Runo system.
RUNO_VERSION = local
RUNO_SITE = $(BR2_EXTERNAL_RUNO_PATH)/package/runo/src
RUNO_SITE_METHOD = local
RUNO_LICENSE = MIT
RUNO_BINARY = $(call qstrip,$(BR2_PACKAGE_RUNO_BINARY))

define RUNO_BUILD_CMDS
	test -f "$(RUNO_BINARY)" || \
		{ echo "runo binary not found: $(RUNO_BINARY)"; exit 1; }
	file -b "$(RUNO_BINARY)" | grep -Eq "ARM aarch64.*static" || \
		{ echo "$(RUNO_BINARY) is not a static aarch64 binary"; exit 1; }
endef

define RUNO_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 "$(RUNO_BINARY)" $(TARGET_DIR)/usr/bin/runo
endef

$(eval $(generic-package))
