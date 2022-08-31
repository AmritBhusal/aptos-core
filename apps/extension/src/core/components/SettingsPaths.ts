// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

import { FaLock } from '@react-icons/all-files/fa/FaLock';
import { FiChevronRight } from '@react-icons/all-files/fi/FiChevronRight';
import Routes from 'core/routes';
import { settingsItemLabel } from 'core/constants';
import { AiFillQuestionCircle } from '@react-icons/all-files/ai/AiFillQuestionCircle';
import { MdWifiTethering } from '@react-icons/all-files/md/MdWifiTethering';
import { Divider } from '@chakra-ui/react';
import { SettingsListItemProps } from './SettingsListItem';

export const signOutSecondaryGridBgColor = {
  dark: 'red.500',
  light: 'red.500',
};

export const signOutSecondaryGridHoverBgColor = {
  dark: 'red.600',
  light: 'red.400',
};

export const secondaryGridHoverTextColor = {
  dark: 'white',
  light: 'black',
};

export default function getSettingsPaths(hasMnemonic: boolean): SettingsListItemProps[] {
  let items: SettingsListItemProps[] = [
    {
      iconAfter: FiChevronRight,
      iconBefore: MdWifiTethering,
      path: Routes.network.path,
      title: settingsItemLabel.NETWORK,
    },
    // {
    //   DividerComponent: Divider,
    //   iconAfter: FiChevronRight,
    //   iconBefore: BsShieldFill,
    //   path: Routes.security_privacy.path,
    //   title: 'Security and Privacy',
    // },
    {
      externalLink: 'https://discord.gg/rGRFrgFT',
      iconBefore: AiFillQuestionCircle,
      path: null,
      title: settingsItemLabel.HELP_SUPPORT,
    },
    {
      DividerComponent: Divider,
      iconBefore: FaLock,
      path: Routes.wallet.path,
      title: settingsItemLabel.LOCK_WALLET,
    },
    {
      iconAfter: FiChevronRight,
      path: Routes.credentials.path,
      title: settingsItemLabel.SHOW_CREDENTIALS,
    },
  ];

  if (hasMnemonic) {
    items.push({
      iconAfter: FiChevronRight,
      path: Routes.recovery_phrase.path,
      title: settingsItemLabel.SECRET_RECOVERY_PHRASE,
    });
  }

  items = items.concat(
    [
      {
        iconAfter: FiChevronRight,
        path: Routes.switchAccount.path,
        title: settingsItemLabel.SWITCH_ACCOUNT,
      },
      {
        path: null,
        textColorDict: {
          dark: 'red.400',
          light: 'red.400',
        },
        title: settingsItemLabel.REMOVE_ACCOUNT,
      },
    ],
  );

  return items;
}
