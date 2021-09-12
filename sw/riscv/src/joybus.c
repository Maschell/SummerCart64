#include "joybus.h"
#include "rtc.h"


#define CMD_EEPROM_STATUS       (0x00)
#define CMD_EEPROM_READ         (0x04)
#define CMD_EEPROM_WRITE        (0x05)
#define CMD_RTC_STATUS          (0x06)
#define CMD_RTC_READ            (0x07)
#define CMD_RTC_WRITE           (0x08)

#define EEPROM_ID_4K            (0x80)
#define EEPROM_ID_16K           (0xC0)
#define RTC_ID                  (0x10)

#define EEPROM_PAGE_SIZE        (8)

#define RTC_STATUS_STOPPED      (0x80)
#define RTC_STATUS_RUNNING      (0x00)
#define RTC_STATUS(running)     (running ? RTC_STATUS_RUNNING : RTC_STATUS_STOPPED)

#define RTC_WP_MASK             (0x03)
#define RTC_ST_MASK             (0x04)
#define RTC_CENTURY_20XX        (0x01)


static void rx (uint8_t *data) {
    uint32_t rx_length = (JOYBUS->SCR & JOYBUS_SCR_RX_LENGTH_MASK) >> JOYBUS_SCR_RX_LENGTH_BIT;
    for (size_t i = 0; i < rx_length; i++) {
        data[i] = ((uint8_t *) JOYBUS->DATA)[(10 - rx_length) + i];
    }
}

static void tx (uint8_t *data, size_t length) {
    for (size_t i = 0; i < ((length + 3) / 4); i++) {
        JOYBUS->DATA[i] = ((uint32_t *) data)[i];
    }
    JOYBUS->DATA[length / 4] |= (0x80 << ((length % 4) * 8));
    JOYBUS->SCR = (((length * 8) + 1) << JOYBUS_SCR_TX_LENGTH_BIT) | JOYBUS_SCR_TX_START;
}

static uint32_t swapb (uint32_t data) {
    return (
        (data << 24) |
        ((data << 8) & 0x00FF0000) |
        ((data >> 8) & 0x0000FF00) |
        (data >> 24)
    );
}


struct process {
    enum eeprom_type eeprom_type;
    bool rtc_running;
    uint8_t rtc_write_protect;
};

static struct process p;


void joybus_set_eeprom (enum eeprom_type eeprom_type) {
    p.eeprom_type = eeprom_type;
}


void joybus_init (void) {
    JOYBUS->SCR = JOYBUS_SCR_TX_RESET | JOYBUS_SCR_RX_RESET;
    p.eeprom_type = EEPROM_NONE;
    p.rtc_running = true;
    p.rtc_write_protect = RTC_WP_MASK;
}


void process_joybus (void) {
    uint8_t rx_data[10];
    uint8_t tx_data[12];
    io32_t *save_data;
    uint32_t *data_offset;

    if (JOYBUS->SCR & JOYBUS_SCR_RX_READY) {
        if (JOYBUS->SCR & JOYBUS_SCR_RX_STOP_BIT) {
            rx(rx_data);

            for (size_t i = 0; i < sizeof(tx_data); i++) {
                tx_data[i] = 0x00;
            }

            if (p.eeprom_type != EEPROM_NONE) {
                save_data = (io32_t *) (SDRAM_BASE + CFG->SAVE_OFFSET + (rx_data[1] * EEPROM_PAGE_SIZE));
                switch (rx_data[0]) {
                    case CMD_EEPROM_STATUS:
                        tx_data[1] = p.eeprom_type == EEPROM_16K ? EEPROM_ID_16K : EEPROM_ID_4K;
                        tx(tx_data, 3);
                        break;

                    case CMD_EEPROM_READ:
                        data_offset = (uint32_t *) (&tx_data[0]);
                        data_offset[0] = swapb(save_data[0]);
                        data_offset[1] = swapb(save_data[1]);
                        tx(tx_data, 8);
                        break;

                    case CMD_EEPROM_WRITE:
                        data_offset = (uint32_t *) (&rx_data[2]);
                        save_data[0] = swapb(data_offset[0]);
                        save_data[1] = swapb(data_offset[1]);
                        tx(tx_data, 1);
                        break;
                }
            }

            switch (rx_data[0]) {
                case CMD_RTC_STATUS:
                    tx_data[1] = RTC_ID;
                    tx_data[2] = RTC_STATUS(p.rtc_running);
                    tx(tx_data, 3);
                    break;

                case CMD_RTC_READ:
                    if (rx_data[1] == 0) {
                        tx_data[0] = p.rtc_write_protect;
                        if (!p.rtc_running) {
                            tx_data[1] = RTC_ST_MASK;
                        }
                    } else if (rx_data[1] == 2) {
                        rtc_time_t *rtc_time = rtc_get_time();
                        tx_data[0] = rtc_time->seconds;
                        tx_data[1] = rtc_time->minutes;
                        tx_data[2] = rtc_time->hours | 0x80;
                        tx_data[4] = rtc_time->weekday - 1;
                        tx_data[3] = rtc_time->day;
                        tx_data[5] = rtc_time->month;
                        tx_data[6] = rtc_time->year;
                        tx_data[7] = RTC_CENTURY_20XX;
                    }
                    tx_data[8] = RTC_STATUS(p.rtc_running);
                    tx(tx_data, 9);
                    break;

                case CMD_RTC_WRITE:
                    if (rx_data[1] == 0) {
                        p.rtc_write_protect = rx_data[2] & RTC_WP_MASK;
                        p.rtc_running = (!(rx_data[3] & RTC_ST_MASK));
                    } else if (rx_data[1] == 2) {
                        rtc_time_t rtc_time;
                        rtc_time.seconds = rx_data[2];
                        rtc_time.minutes = rx_data[3];
                        rtc_time.hours = rx_data[4] & 0x7F;
                        rtc_time.weekday = rx_data[6] + 1;
                        rtc_time.day = rx_data[5];
                        rtc_time.month = rx_data[7];
                        rtc_time.year = rx_data[8];
                        rtc_set_time(&rtc_time);
                    }
                    tx_data[0] = RTC_STATUS(p.rtc_running);
                    tx(tx_data, 1);
                    break;
            }
        }

        JOYBUS->SCR = JOYBUS_SCR_RX_RESET;
    }
}